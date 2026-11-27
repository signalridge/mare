//! Windows sandbox, default tier: a confined command can write inside the
//! workspace and its private temp directory but not elsewhere in the user's profile; PowerShell
//! pipelines work under the restricted token (default DACL); a restricted network isn't controlled
//! there, so the permissions plugin accepts it by default and reports `partial` / PARTIAL.
//!
//! Needs real Windows — Wine does not enforce restricted tokens or ACLs — so it runs on the
//! `windows-latest` CI job (or locally with MARE_WIN_SANDBOX_TEST=1 on Windows).
#![cfg(windows)]

use crate::common::*;

#[tokio::test]
async fn default_tier_confines_writes_runs_powershell_and_bridges_conpty() {
    if std::env::var("CI").is_err() && std::env::var("MARE_WIN_SANDBOX_TEST").is_err() {
        eprintln!("skipped: set MARE_WIN_SANDBOX_TEST=1 to run on a local Windows machine");
        return;
    }
    local(inner()).await
}

async fn inner() {
    // the runner is the mare binary itself (`mare __sandbox-exec …`)
    let mut env = TestEnv::new().await;
    env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
    let cwd = env.cwd.clone();
    // outside the workspace, normally writable by the user
    let outside = std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap())
        .join(format!("mare-sbx-outside-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&outside);
    let h = env.host().build().await;
    h.lua().globals().set("CWD", cwd.to_string_lossy().to_string()).unwrap();
    h.lua().globals().set("OUTSIDE", outside.to_string_lossy().to_string()).unwrap();
    let (mech, enforcement, inside_code, outside_code, temp_code, ps_out, failed): (
        String,
        String,
        i64,
        i64,
        i64,
        String,
        Option<String>,
    ) = eval(
        &h,
        r#"
        local function run(args)
            return mare.proc.run({ cmd = "cmd.exe", args = args, cwd = CWD, sandbox = { level = "workspace-write", network = true, accept_partial = { "fs_protect" } }, timeout_ms = 60000 })
        end
        local inside = run({ "/c", "echo in> inside.txt" })
        local outside = run({ "/c", "echo out> " .. OUTSIDE })
        local temp = run({ "/c", "echo t> %TEMP%\\t.txt && type %TEMP%\\t.txt" })
        local ps = mare.proc.run({
            cmd = "powershell.exe",
            args = { "-NoProfile", "-NonInteractive", "-Command", "1..3 | ForEach-Object { $_ * 2 } | Out-String" },
            cwd = CWD, sandbox = { level = "workspace-write", network = true, accept_partial = { "fs_protect" } }, timeout_ms = 120000,
        })
        assert(table.concat(inside.sandbox.enforced, ","):find("fs_write", 1, true))
        assert(table.concat(inside.sandbox.unavailable, ","):find("fs_protect", 1, true))
        return inside.sandbox.mechanism, inside.sandbox.enforcement, inside.code, outside.code, temp.code,
            ps.stdout, inside.sandbox_failed or outside.sandbox_failed or ps.sandbox_failed
    "#,
    )
    .await;
    assert_eq!(failed, None, "the sandbox failed to start");
    assert_eq!(mech, "windows-acl");
    // File writes are confined; metadata protection is the explicitly accepted default-tier gap.
    assert_eq!(enforcement, "partial", "the metadata protection gap must remain visible");
    assert_eq!(inside_code, 0, "a write inside the workspace must succeed");
    assert!(cwd.join("inside.txt").exists());
    assert_ne!(outside_code, 0, "a write outside the workspace must fail");
    assert!(!outside.exists(), "the confined command wrote outside the workspace: {}", outside.display());
    assert_eq!(temp_code, 0, "the private temp directory must be writable");
    assert!(ps_out.contains('6'), "PowerShell pipeline under the restricted token: {ps_out:?}");

    // a terminal under the policy: the runner owns the ConPTY and bridges it —
    // output comes back, input reaches the program, and writes stay confined
    let pty_outside = std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap())
        .join(format!("mare-sbx-pty-outside-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&pty_outside);
    h.lua().globals().set("PTY_OUTSIDE", pty_outside.to_string_lossy().to_string()).unwrap();
    let (seen, mech): (String, String) = eval(
        &h,
        r#"
        local p = mare.pty.spawn({ cmd = "cmd.exe", cols = 100, rows = 30, sandbox = { level = "workspace-write", network = true, accept_partial = { "fs_protect" } } })
        p:resize(120, 40)
        p:write("echo ok> in-pty.txt\r\n")
        p:write("echo x> " .. PTY_OUTSIDE .. "\r\n")
        p:write("echo pty-done\r\n")
        p:write("exit\r\n")
        local seen = ""
        while true do
            local chunk = p:read()
            if not chunk then break end
            seen = seen .. chunk
        end
        return seen, p:sandbox().mechanism
    "#,
    )
    .await;
    assert_eq!(mech, "windows-acl");
    assert!(seen.contains("pty-done"), "no output bridged back from the ConPTY: {seen:?}");
    assert!(cwd.join("in-pty.txt").exists(), "input didn't reach the confined program: {seen:?}");
    assert!(!pty_outside.exists(), "the confined terminal wrote outside the workspace: {seen:?}");
    h.shutdown().await;
}

#[tokio::test]
async fn default_tier_runs_restricted_network_as_partial_via_permissions() {
    if std::env::var("CI").is_err() && std::env::var("MARE_WIN_SANDBOX_TEST").is_err() {
        eprintln!("skipped: set MARE_WIN_SANDBOX_TEST=1 to run on a local Windows machine");
        return;
    }
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
        let mut b = env.host().no_session().config(serde_json::json!({ "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        let mech: String = eval(&h, "return mare.proc.sandbox.capabilities().mechanism").await;
        if mech != "windows-acl" {
            // the enhanced tier is set up on this machine: the network is controlled (full)
            return h.shutdown().await;
        }
        expose(&h);
        global(&h, "P", h.service_table("permissions").unwrap());
        let (code, enforcement, label): (i64, String, String) = eval(
            &h,
            r#"local label
               C.cordis:on("permission/changed", function(ev) label = ev.label end)
               P:set("read-only")
               local r = mare.proc.run({ cmd = "cmd.exe", args = { "/c", "exit 0" }, sandbox = "current", timeout_ms = 60000 })
               return r.code, r.sandbox.enforcement, tostring(label)"#,
        )
        .await;
        assert_eq!((code, enforcement.as_str(), label.as_str()), (0, "partial", "PARTIAL"));
        h.shutdown().await;
    })
    .await
}

/// A project under %TEMP% (an extracted archive, a temp clone) with the default permissions:
/// untrusted, so the read-only preset; an approved write runs with the workspace as its extra
/// writable root and the file lands. Every such write failed with SANDBOX_INVALID_ROOT while
/// the root check compared std's `\\?\C:\…` canonical form with the plain path (CI run
/// 42565812901). The bash variant (a `write:` scope for a second directory) is the next test.
#[tokio::test]
async fn an_approved_write_lands_in_a_project_under_temp() {
    if std::env::var("CI").is_err() && std::env::var("MARE_WIN_SANDBOX_TEST").is_err() {
        eprintln!("skipped: set MARE_WIN_SANDBOX_TEST=1 to run on a local Windows machine");
        return;
    }
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
        let temp = dunce::canonicalize(std::env::temp_dir()).unwrap();
        assert!(env.cwd.starts_with(&temp), "{} is not under %TEMP% ({})", env.cwd.display(), temp.display());
        let cwd = env.cwd.clone();
        let h = env
            .host()
            .no_session()
            .config(serde_json::json!({ "plugins": { "permissions": { "enabled": true } } }))
            .build()
            .await;
        expose(&h);
        let (content, asks): (String, i64) = eval(
            &h,
            r#"local asks = 0
               local r = T:execute({ id = "1", name = "write", arguments = { path = "a.txt", content = "A" } }, {
                 approve = function() asks = asks + 1 return { decision = "allow", source = "user" } end,
               })
               return tostring(r.content), asks"#,
        )
        .await;
        assert_eq!(asks, 1, "a write under the untrusted default preset asks once: {content}");
        assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).ok().as_deref(), Some("A"), "{content}");
        assert!(!content.contains("SANDBOX_"), "{content}");
        h.shutdown().await;
    })
    .await
}

/// Part two of the test above: a trusted project (`auto`) approving a `write:` scope for a second
/// directory under %TEMP% goes through the same root check. Not runnable under the default tier:
/// the bash tool runs Git Bash, whose MSYS runtime can't initialise under the write-restricted
/// token (`couldn't create signal pipe, Win32 error 5`, exit 0xC0000142 — CI run 42570238867);
/// by design the tool reports SANDBOX_MSYS_UNSUPPORTED there (see
/// `git_bash_under_the_sandbox_works_or_reports_msys_unsupported`) and Git Bash needs the
/// enhanced tier (`enhanced_tier_runs_git_bash_and_keeps_its_own_files_read_only`).
#[tokio::test]
#[ignore = "by design: no Git Bash under the default Windows tier"]
async fn an_approved_bash_write_lands_in_a_second_directory_under_temp() {
    if std::env::var("CI").is_err() && std::env::var("MARE_WIN_SANDBOX_TEST").is_err() {
        eprintln!("skipped: set MARE_WIN_SANDBOX_TEST=1 to run on a local Windows machine");
        return;
    }
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
        // trusted (`auto`): a command asking for a second directory under %TEMP%
        let other = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(other.path()).unwrap();
        let mut b =
            env.host().no_session().config(serde_json::json!({ "plugins": { "permissions": { "enabled": true } } }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "OTHER", other.to_string_lossy().to_string());
        let (content, asks): (String, i64) = eval(
            &h,
            r#"local asks = 0
               local r = T:execute({ id = "2", name = "bash", arguments = {
                 command = 'echo B> "' .. OTHER:gsub("\\", "/") .. '/b.txt"',
                 sandbox_permissions = "write:" .. OTHER,
                 justification = "test: write the second directory",
               } }, {
                 approve = function() asks = asks + 1 return { decision = "allow", source = "user" } end,
               })
               return tostring(r.content), asks"#,
        )
        .await;
        assert_eq!(asks, 1, "{content}");
        assert!(!content.contains("SANDBOX_"), "{content}");
        assert_eq!(
            std::fs::read_to_string(other.join("b.txt")).ok().map(|s| s.trim().to_string()).as_deref(),
            Some("B"),
            "{content}"
        );
        h.shutdown().await;
    })
    .await
}

/// (2026-11-09): a workspace that contains %TEMP% (mare started in the home folder,
/// at `C:\` or in %TEMP%) is not refused; the private temp directory (`%TEMP%\mare-sbx\…`) uses
/// the workspace's own write access, and `/permission caps` and the sandbox report say the temp
/// directory is not separately isolated. Here TEMP/TMP point inside the workspace.
#[tokio::test]
async fn an_approved_write_lands_in_a_workspace_containing_temp() {
    if std::env::var("CI").is_err() && std::env::var("MARE_WIN_SANDBOX_TEST").is_err() {
        eprintln!("skipped: set MARE_WIN_SANDBOX_TEST=1 to run on a local Windows machine");
        return;
    }
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
        let cwd = env.cwd.clone();
        let temp = cwd.join("tmp");
        std::fs::create_dir_all(&temp).unwrap();
        env.set_var("TMP", &temp);
        env.set_var("TEMP", &temp);
        // the private temp directory is chosen once per process (nextest: one process per test)
        let private = mare_binding::winsandbox::session_temp_dir().unwrap();
        assert!(private.starts_with(&temp), "{} is not under TEMP={}", private.display(), temp.display());
        assert!(matches!(
            mare_binding::winsandbox::session_temp_grant(std::slice::from_ref(&cwd)),
            Ok(mare_binding::winsandbox::TempGrant::Shared(_))
        ));

        let h =
            env.host().no_session().config(serde_json::json!({ "plugins": { "permissions": { "enabled": true } } })).build().await;
        expose(&h);
        exec(&h, r#"N = {}; mare.ui.notify = function(t) N[#N + 1] = tostring(t) end"#).await;
        // untrusted: the read-only preset; the approved write runs with the workspace writable
        let (content, asks): (String, i64) = eval(
            &h,
            r#"local asks = 0
               local r = T:execute({ id = "1", name = "write", arguments = { path = "a.txt", content = "A" } }, {
                 approve = function() asks = asks + 1 return { decision = "allow", source = "user" } end,
               })
               return tostring(r.content), asks"#,
        )
        .await;
        assert_eq!(asks, 1, "a write under the untrusted default preset asks once: {content}");
        assert!(!content.contains("SANDBOX_") && !content.contains("overlap"), "{content}");
        assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).ok().as_deref(), Some("A"), "{content}");

        // a confined command writes its private temp directory through the workspace's grant,
        // and the report says the temp directory is not separately isolated
        let (code, note, failed): (i64, String, Option<String>) = eval(
            &h,
            r#"local r = mare.proc.run({ cmd = "cmd.exe", args = { "/c", "echo t> %TEMP%\\t.txt && type %TEMP%\\t.txt" },
                 cwd = mare.cwd, sandbox = { level = "workspace-write", network = true, accept_partial = { "fs_protect" } }, timeout_ms = 60000 })
               assert(r.sandbox.enforcement == "partial")
               assert(table.concat(r.sandbox.enforced, ","):find("fs_write", 1, true))
               assert(table.concat(r.sandbox.unavailable, ","):find("fs_protect", 1, true))
               return r.code, tostring(r.sandbox and r.sandbox.temp), r.sandbox_failed"#,
        )
        .await;
        assert_eq!(failed, None, "the sandbox failed to start");
        assert_eq!(code, 0, "the private temp directory must be writable");
        assert!(private.join("t.txt").exists(), "the command's TEMP is the private temp directory");
        assert!(note.contains("temp directory not separately isolated"), "{note}");

        global(&h, "CM", h.service_table("commands").unwrap());
        exec(&h, r#"CM:run("/permission caps")"#).await;
        let shown: String = eval(&h, "return table.concat(N, '\\n')").await;
        assert!(shown.contains("temp directory not separately isolated"), "{shown}");
        h.shutdown().await;
    })
    .await
}

/// The bash tool (Git Bash) under the default tier: MSYS can't start under the write-restricted
/// token, so the result is the coded SANDBOX_MSYS_UNSUPPORTED
/// naming the ways out, never the raw runtime crash; the shell model commands default to is
/// PowerShell there, and Git Bash again without the sandbox. (The enhanced tier runs Git Bash:
/// `enhanced_tier_runs_git_bash_and_keeps_its_own_files_read_only`.)
#[tokio::test]
async fn git_bash_under_the_sandbox_works_or_reports_msys_unsupported() {
    if std::env::var("CI").is_err() && std::env::var("MARE_WIN_SANDBOX_TEST").is_err() {
        eprintln!("skipped: set MARE_WIN_SANDBOX_TEST=1 to run on a local Windows machine");
        return;
    }
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
        let mut b =
            env.host().no_session().config(serde_json::json!({ "plugins": { "permissions": { "enabled": true } } }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        let (content, is_error, code): (String, bool, Option<String>) = eval(
            &h,
            r#"local r = T:execute({ id = "1", name = "bash", arguments = { command = "echo msys-ok" } }, {
                 approve = function() return { decision = "allow", source = "user" } end,
               })
               return tostring(r.content), r.is_error == true, r.error_code"#,
        )
        .await;
        let mech: String = eval(&h, "return mare.proc.sandbox.capabilities().mechanism").await;
        if mech == "windows-acl" {
            assert!(is_error, "Git Bash started under the default tier: {content}");
        }
        if is_error {
            assert!(content.starts_with("SANDBOX_MSYS_UNSUPPORTED: "), "{content}");
            assert_eq!(code.as_deref(), Some("sandbox_msys_unsupported"));
            assert!(content.contains("powershell") && content.contains("mare sandbox setup"), "{content}");
        } else {
            assert!(content.contains("msys-ok"), "{content}");
        }
        assert!(!content.contains("fatal error"), "the raw MSYS crash reached the model: {content}");

        // the default shell for model commands: PowerShell inside the default tier, Git Bash
        // outside the sandbox
        global(&h, "P", h.service_table("permissions").unwrap());
        let (sandboxed, blocked, unsandboxed): (String, bool, String) = eval(
            &h,
            r#"local a = T:shell()
               P:set("full-access")
               local b = T:shell()
               return a.kind, a.msys_blocked == true, b.kind"#,
        )
        .await;
        if mech == "windows-acl" {
            assert_eq!((sandboxed.as_str(), blocked), ("powershell", true));
        } else {
            assert_eq!(sandboxed, "bash");
        }
        assert_eq!(unsandboxed, "bash", "outside the sandbox Git Bash is the default again");
        h.shutdown().await;
    })
    .await
}

/// enhanced tier (needs `mare sandbox setup`; CI measures its station and
/// requires either genuine enhanced execution or the explicit unsupported-station diagnosis): the inner runner adds the sandbox account's own SID to the restricting
/// SIDs, so Git Bash starts — and the account still can't write `%ProgramData%\mare\sandbox`, its
/// `bin`, the helper, or the user's profile.
#[test]
fn enhanced_tier_runs_git_bash_and_keeps_its_own_files_read_only() {
    use mare_binding::{winsandbox, winsandbox_user as wsu};
    use std::process::Stdio;
    let required = std::env::var_os("MARE_REQUIRE_ENHANCED_SANDBOX").is_some();
    let unavailable_station = std::env::var("MARE_EXPECT_ENHANCED_UNAVAILABLE").ok();
    let dir = wsu::sandbox_dir();
    let state = match wsu::SetupState::load(&dir) {
        Ok(Some(s)) if s.version == wsu::SETUP_VERSION => s,
        _ => {
            assert!(
                !required && unavailable_station.is_none(),
                "enhanced CI requires current setup; run `mare sandbox setup` first"
            );
            eprintln!("skipped: the enhanced tier is not set up (`mare sandbox setup`)");
            return;
        }
    };
    // what `mare doctor` checks: only SYSTEM and Administrators may write there
    wsu::win::check_lock(&state).expect("setup locks the sandbox directory, bin and helper");
    let guard = tempfile::tempdir().unwrap();
    let ws = dunce::canonicalize(guard.path()).unwrap();
    let temp = winsandbox::session_temp_dir().unwrap();
    let sids = winsandbox::prepare_grants(std::slice::from_ref(&ws), &temp).unwrap();
    wsu::win::grant_group(&state.group_sid, &[ws.clone(), temp.clone()], &[]).unwrap();
    // the chain commands take: the outer runner (real user) → the helper as mare-sbx-off
    let run_mode = |argv: &[&str], pty: Option<(u16, u16)>, readonly: bool, bulk_env: bool| -> (i32, String) {
        let ra = winsandbox::RunnerArgs {
            mode: if readonly { "read-only" } else { "workspace-write" }.into(),
            sids: if readonly { vec![winsandbox::readonly_capability_sid()] } else { sids.clone() },
            cwd: ws.clone(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            as_user: Some("offline".into()),
            account: None,
            pty,
        };
        let mut cmd = mare_cmd();
        cmd.env("MARE_IPC_ENV_TEST", "mare-env-世界");
        // Only the large-IPC payload test injects >64 KiB of env vars; PowerShell 5.1 `Add-Type`
        // (`csc.exe` via .NET `Process.Start`) rejects environment blocks over 65535 bytes.
        if bulk_env {
            cmd.envs((0..16).map(|index| (format!("MARE_IPC_BULK_{index}"), "x".repeat(4096))));
        }
        let o = cmd
            .arg(winsandbox::RUNNER_ARG)
            .args(ra.to_args())
            .env("TEMP", &temp)
            .env("TMP", &temp)
            .current_dir(&ws)
            .stdin(Stdio::null())
            .run_output();
        let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        (o.status.code().unwrap_or(-1), text)
    };
    let run_with_pty = |argv: &[&str], pty| run_mode(argv, pty, false, false);
    let run = |argv: &[&str]| run_with_pty(argv, None);
    // The enhanced helper cannot start in an SSH/service window station. Verify the
    // explicit pre-launch refusal for both transports; this does not exercise enhanced
    // confinement and must never satisfy the dedicated interactive-session CI step.
    let status = mare_cmd().args(["sandbox", "status", "--require-full"]).run_output();
    let status_text = format!("{}{}", String::from_utf8_lossy(&status.stdout), String::from_utf8_lossy(&status.stderr));
    // Run the production binary's gate: this test harness has a different executable hash.
    // A passed self-test still cannot satisfy --require-full while per-call network is partial.
    assert!(!status.status.success(), "partial network isolation satisfied --require-full: {status_text}");
    if status_text.contains("enhanced sandbox requires WinSta0; parent window station is ") {
        let why = status_text.as_str();
        if let Some(station) = &unavailable_station {
            assert!(why.contains(&format!("parent window station is {station};")), "{why}");
        }
        assert!(!status.status.success(), "unavailable enhanced tier reported success: {status_text}");
        assert!(status_text.contains("using the default tier (enforcement: partial)"), "{status_text}");
        let marker = ws.join("enhanced-must-not-run.txt");
        for pty in [None, Some((120, 40))] {
            let (code, out) = run_with_pty(&["cmd.exe", "/c", "echo ran> enhanced-must-not-run.txt"], pty);
            assert_ne!(code, 0, "enhanced runner ran in an unsupported station: {out}");
            assert!(out.contains("enhanced sandbox requires WinSta0; parent window station is "), "{out}");
            assert!(!marker.exists(), "payload ran despite unavailable tier: {out}");
        }
        assert!(!required, "enhanced CI requires an interactive WinSta0 session: {why}");
        eprintln!(
            "verified enhanced tier unavailable in this station: {why}; pipe/ConPTY confinement requires WinSta0"
        );
        return;
    }
    assert!(unavailable_station.is_none(), "expected unavailable tier on {unavailable_station:?}");
    assert!(status_text.contains("self-test: passed"), "production mare self-test failed: {status_text}");
    assert!(status_text.contains("enforcement: partial"), "{status_text}");
    std::fs::write(ws.join("readonly-input.txt"), "readonly-content").unwrap();
    for (index, pty) in [None, Some((120, 40))].into_iter().enumerate() {
        let (code, out) = run_mode(&["cmd.exe", "/c", "type readonly-input.txt"], pty, true, false);
        assert_eq!(code, 0, "enhanced read-only launch failed: {out}");
        assert!(out.contains("readonly-content"), "{out}");
        let marker = format!("readonly-must-not-write-{index}.txt");
        let command = format!("echo forbidden> {marker}");
        let (code, out) = run_mode(&["cmd.exe", "/c", &command], pty, true, false);
        assert_ne!(code, 0, "read-only payload wrote the workspace: {out}");
        assert!(!ws.join(marker).exists(), "read-only created a file: {out}");
    }
    // Query the actual confined process's desktop, twice across separate logins. This
    // exercises the complete outer runner → account runner → restricted child chain.
    let desktop_script = r#"
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public class MareDesktop {
 [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
 [DllImport("user32.dll")] public static extern IntPtr GetThreadDesktop(uint thread);
 [DllImport("user32.dll")] public static extern IntPtr GetProcessWindowStation();
 [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
 public static extern bool GetUserObjectInformation(IntPtr h, int index, StringBuilder value, uint size, out uint needed);
 public static string Name(IntPtr h) {
  var value = new StringBuilder(256); uint needed;
  if (!GetUserObjectInformation(h, 2, value, 512, out needed)) throw new Exception("desktop query failed");
  return value.ToString();
 }
}
'@
Write-Output ('MARE-DESKTOP=' + [MareDesktop]::Name([MareDesktop]::GetProcessWindowStation()) + '\' + [MareDesktop]::Name([MareDesktop]::GetThreadDesktop([MareDesktop]::GetCurrentThreadId())))
"#;
    let desktop_name = |pty| {
        let (code, out) = run_with_pty(
            &["powershell.exe", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", desktop_script],
            pty,
        );
        assert_eq!(code, 0, "enhanced PowerShell launch failed: {out}");
        let name: String = out
            .split("MARE-DESKTOP=")
            .nth(1)
            .expect(&out)
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '\\')
            .collect();
        assert!(name.to_ascii_lowercase().starts_with("winsta0\\mare-sandbox-"), "{name}");
        name
    };
    // Exercise a real payload larger than CreateProcessWithLogonW's 1024-character limit.
    // The account-runner transport must carry it intact, including Unicode environment values.
    let expected = "mare-ipc-世界-".repeat(256);
    let long_script = format!(
        "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); Write-Output '{expected}'; Write-Output $env:MARE_IPC_ENV_TEST"
    );
    assert!(long_script.encode_utf16().count() > 1024);
    let (code, out) = run_mode(
        &["powershell.exe", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", &long_script],
        None,
        false,
        true,
    );
    assert_eq!(code, 0, "enhanced long payload failed: {out}");
    assert!(out.contains(&expected), "long payload was truncated: {out}");
    assert!(out.contains("mare-env-世界"), "request environment was lost: {out}");

    let first = desktop_name(None);
    assert_ne!(first, desktop_name(None), "enhanced pipe commands reused a desktop");
    assert_ne!(first, desktop_name(Some((120, 40))), "enhanced ConPTY command reused a desktop");

    let fwd = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/");
    let pid = std::process::id();
    let bash = std::path::PathBuf::from(std::env::var("ProgramFiles").unwrap()).join(r"Git\bin\bash.exe");
    let bash = bash.is_file().then(|| bash.to_string_lossy().into_owned());

    // Git Bash runs, writes the workspace, and still can't write the user's profile
    if let Some(bash) = &bash {
        let (code, out) = run(&[bash, "-c", "echo msys-ok; echo x > ws-bash.txt && echo ws-ok"]);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("msys-ok") && out.contains("ws-ok"), "{out}");
        assert!(ws.join("ws-bash.txt").exists(), "{out}");
        let profile =
            std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap()).join(format!("mare-enh-{pid}.txt"));
        let (code, out) = run(&[bash, "-c", &format!("echo x > '{}'", fwd(&profile))]);
        let leaked = profile.exists();
        let _ = std::fs::remove_file(&profile);
        assert!(code != 0 && !leaked, "Git Bash wrote the user's profile: {out}");
    }

    // the sandbox directory and its bin: no new files, from cmd or from Git Bash
    for d in [dir.clone(), dir.join("bin")] {
        let f = d.join(format!("mare-enh-probe-{pid}.txt"));
        let (code, out) = run(&["cmd.exe", "/c", &format!("echo x> \"{}\"", f.display())]);
        let leaked = f.exists();
        let _ = std::fs::remove_file(&f);
        assert!(code != 0 && !leaked, "the sandbox account wrote {}: {out}", f.display());
        if let Some(bash) = &bash {
            let (code, out) = run(&[bash, "-c", &format!("echo x > '{}'", fwd(&f))]);
            let leaked = f.exists();
            let _ = std::fs::remove_file(&f);
            assert!(code != 0 && !leaked, "Git Bash wrote {}: {out}", f.display());
        }
    }

    // the helper: neither its DACL nor its bytes change
    let before = wsu::sha256_file(&state.helper).unwrap();
    let helper = state.helper.to_string_lossy().into_owned();
    let (code, out) = run(&["icacls.exe", &helper, "/grant", "*S-1-1-0:F"]);
    assert_ne!(code, 0, "the sandbox account changed the helper's DACL: {out}");
    if let Some(bash) = &bash {
        let (code, out) = run(&[bash, "-c", &format!("echo x >> '{}'", fwd(&state.helper))]);
        assert_ne!(code, 0, "Git Bash appended to the helper: {out}");
    }
    assert_eq!(wsu::sha256_file(&state.helper).unwrap(), before, "the helper changed");
    wsu::win::check_lock(&state).expect("still locked after the probes");
}
