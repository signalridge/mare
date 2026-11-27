//! Sandbox backend confinement, terminals, network proxy, capabilities, and doctor checks.

use super::{host, mechanism};
use crate::common::*;
use crate::suite::sandbox_expect::require_backend;

#[tokio::test]
async fn writes_outside_the_workspace_are_structured_denials() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        if !require_backend(&h, "writes_outside_the_workspace_are_structured_denials", mare_binding::ossandbox::expect::ANY).await { return h.shutdown().await; }
        let (code, denied, marker, enforcement, plain): (i64, String, String, String, bool) = eval(
            &h,
            r#"local a = mare.proc.run({ cmd = "sh", args = { "-c", "echo hi > in-any.txt" }, sandbox = "workspace-write" })
               local b = mare.proc.run({ cmd = "sh", args = { "-c", "echo x > /usr/mare-sbx-x" }, sandbox = "workspace-write" })
               local c = mare.proc.run({ cmd = "sh", args = { "-c", "ls /nope" }, sandbox = "workspace-write" })
               return a.code, tostring(b.sandbox_denied), tostring(b.sandbox_marker), a.sandbox.enforcement, c.sandbox_denied == nil"#,
        )
        .await;
        assert_eq!(code, 0);
        assert!(!std::path::Path::new("/usr/mare-sbx-x").exists());
        // the marker names the refused path (what the user and the model see)
        assert_eq!(
            (denied.as_str(), marker.as_str()),
            ("write", "[sandbox: file access denied under workspace-write mode: write /usr/mare-sbx-x]")
        );
        assert!(enforcement == "full" || enforcement == "partial");
        assert!(plain, "an ordinary failure is not a sandbox denial");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_terminal_under_a_policy_is_confined() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let mech = mechanism(&h).await;
        if !require_backend(&h, "a_terminal_under_a_policy_is_confined", mare_binding::ossandbox::expect::ANY).await { return h.shutdown().await; }
        let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let outside_dir = dunce::canonicalize(outside.path()).unwrap().join("with space");
        std::fs::create_dir(&outside_dir).unwrap();
        let out = outside_dir.join("pty-escape.txt");
        let unconfined_out = outside_dir.join("unconfined-escape.txt");
        global(&h, "OUTSIDE", out.to_string_lossy().to_string());
        global(&h, "UNCONFINED_OUTSIDE", unconfined_out.to_string_lossy().to_string());
        // a Unix socket from inside the terminal (when python3 is there to try): the policy's
        // network is restricted, so on Linux the seccomp filter refuses it in the PTY child too
        global(
            &h,
            "SOCK",
            "command -v python3 >/dev/null && { python3 -c 'import socket; socket.socket(socket.AF_UNIX)' 2>/dev/null && echo SOCK-OPEN || echo SOCK-REFUSED; }",
        );
        let (ok, msg): (bool, String) = eval(
            &h,
            r#"local ok, p = pcall(mare.pty.spawn, { cmd = "sh", args = { "-c", 'echo ok > in-pty.txt; echo x > "$1"; ' .. SOCK .. '; echo done', "sh", OUTSIDE },
                   sandbox = { level = "workspace-write", exclude_tmpdir_env_var = true, exclude_slash_tmp = true } })
               if not ok then return false, tostring(p) end
               local seen = ""
               while true do
                   local chunk = p:read()
                   if not chunk then break end
                   seen = seen .. chunk
               end
               return true, seen"#,
        )
        .await;
        // every backend confines it: Seatbelt / bubblewrap wrap it inside the PTY, Landlock and
        // bubblewrap's Unix-socket filter go on in the terminal's pre-exec step
        assert!(ok && msg.contains("done"), "{mech}: {msg}");
        assert!(env.cwd.join("in-pty.txt").exists(), "pty child couldn't write inside the workspace: {msg}");
        assert!(!out.exists(), "pty child escaped the sandbox: {msg}");
        if cfg!(target_os = "linux") {
            assert!(!msg.contains("SOCK-OPEN"), "{mech}: the pty child opened a Unix socket: {msg}");
        }
        // positive control: an unconfined PTY command creates the target path containing spaces
        let (u_ok, u_msg): (bool, String) = eval(
            &h,
            r#"local ok, p = pcall(mare.pty.spawn, { cmd = "sh", args = { "-c", 'echo x > "$1"; echo done', "sh", UNCONFINED_OUTSIDE },
                   sandbox = "danger-full-access" })
               if not ok then return false, tostring(p) end
               local seen = ""
               while true do
                   local chunk = p:read()
                   if not chunk then break end
                   seen = seen .. chunk
               end
               return true, seen"#,
        )
        .await;
        assert!(u_ok && u_msg.contains("done"), "unconfined pty failed: {u_msg}");
        assert!(unconfined_out.exists(), "unconfined pty did not create target with spaces: {u_msg}");
        h.shutdown().await;
    })
    .await
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bwrap_pty_keeps_controlling_terminal_for_dev_tty_sigint_and_job_control() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        if !require_backend(&h, "bwrap_pty_keeps_controlling_terminal_for_dev_tty_sigint_and_job_control", &["bwrap"])
            .await
        {
            return h.shutdown().await;
        }
        // 1. /dev/tty must be openable inside a bwrap-confined PTY.
        let devtty: String = eval(
            &h,
            r#"local p = mare.pty.spawn({
                   cmd = "sh",
                   args = { "-c", "exec 3<>/dev/tty 2>&1 && echo DEVTTY-OK || echo DEVTTY-FAIL" },
                   sandbox = "workspace-write",
               })
               local seen = ""
               while true do
                   local chunk = p:read()
                   if not chunk then break end
                   seen = seen .. chunk
               end
               return seen"#,
        )
        .await;
        assert!(devtty.contains("DEVTTY-OK") && !devtty.contains("DEVTTY-FAIL"), "bwrap PTY lost /dev/tty:\n{devtty}");

        // 2. Writing Ctrl-C (\x03) to the PTY must deliver SIGINT to the foreground process.
        // Run the trapped child as an interactive foreground job so bash places it in its own
        // foreground process group (`tcsetpgrp`): ^C delivers SIGINT exclusively to that
        // foreground job instead of killing outer `bwrap --die-with-parent` (which would SIGKILL
        // the sandbox before the trap runs), and blocking on builtin `read` avoids any external
        // `sleep` fork/exec or fixed-timeout race.
        let sigint: String = eval(
            &h,
            r#"local p = mare.pty.spawn({
                   cmd = "bash",
                   args = {
                       "--norc",
                       "--noprofile",
                       "-ic",
                       "sh -c 'trap \"echo CAUGHT-SIGINT; exit 0\" INT; echo READY; while read _line; do :; done'",
                   },
                   sandbox = "workspace-write",
               })
               local seen = ""
               local sent = false
               while true do
                   local chunk = p:read()
                   if not chunk then break end
                   seen = seen .. chunk
                   if not sent and seen:find("READY", 1, true) then
                       sent = true
                       p:write("\x03")
                   end
               end
               return seen"#,
        )
        .await;
        assert!(sigint.contains("CAUGHT-SIGINT"), "bwrap PTY did not deliver ^C SIGINT:\n{sigint}");

        // 3. Interactive shell job control must stay enabled (no "no job control" warning).
        let jobctl: String = eval(
            &h,
            r#"local p = mare.pty.spawn({
                   cmd = "bash",
                   args = {
                       "--norc",
                       "--noprofile",
                       "-ic",
                       "case $- in *m*) echo JOBCTL-ON ;; *) echo JOBCTL-OFF ;; esac; exit 0",
                   },
                   sandbox = "workspace-write",
               })
               local seen = ""
               while true do
                   local chunk = p:read()
                   if not chunk then break end
                   seen = seen .. chunk
               end
               return seen"#,
        )
        .await;
        assert!(
            jobctl.contains("JOBCTL-ON")
                && !jobctl.contains("JOBCTL-OFF")
                && !jobctl.contains("no job control")
                && !jobctl.contains("cannot set terminal process group"),
            "bwrap PTY broke shell job control:\n{jobctl}"
        );

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn seatbelt_reports_start_failures_and_denied_paths() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        if !require_backend(&h, "seatbelt_reports_start_failures_and_denied_paths", &["seatbelt"]).await { return h.shutdown().await; }
        let (failed, no_denial): (String, bool) = eval(
            &h,
            r#"local r = mare.proc.run({ cmd = "true", sandbox = { profile = "(version 1)(this-is-not-sbpl)" } })
               return tostring(r.sandbox_marker), r.sandbox_denied == nil"#,
        )
        .await;
        assert!(failed.starts_with("[sandbox: failed to start (seatbelt)"), "{failed}");
        assert!(no_denial, "a failed start is not a denial");
        let outside = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(format!(".mare-sbx-{}", std::process::id()));
        global(&h, "OUTSIDE", outside.to_string_lossy().to_string());
        let (denied, path, enforced): (String, String, String) = eval(
            &h,
            r#"local b = mare.proc.run({ cmd = "touch", args = { OUTSIDE }, sandbox = { level = "workspace-write" } })
               return tostring(b.sandbox_denied), tostring(b.denial and b.denial.path), table.concat(b.sandbox.enforced, ",")"#,
        )
        .await;
        let leaked = outside.exists();
        let _ = std::fs::remove_file(&outside);
        assert!(!leaked, "write escaped the sandbox");
        assert_eq!((denied.as_str(), path.as_str()), ("write", outside.to_str().unwrap()));
        assert!(enforced.contains("write"), "{enforced}");
        let (code, report): (i64, bool) =
            eval(&h, r#"local r = mare.proc.run({ cmd = "true", sandbox = { level = "full-access" } }) return r.code, r.sandbox == nil"#).await;
        assert!(code == 0 && report, "an explicit opt-out runs unsandboxed and reports nothing");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn seatbelt_network_goes_through_the_domain_filtering_proxy() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        if !require_backend(&h, "seatbelt_network_goes_through_the_domain_filtering_proxy", &["seatbelt"]).await { return h.shutdown().await; }
        let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = srv.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for mut s in srv.incoming().flatten() {
                let mut b = [0u8; 1024];
                let _ = s.read(&mut b);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
            }
        });
        global(&h, "URL", format!("http://127.0.0.1:{port}/"));
        let (blocked, host_, allowed, bypass): (String, String, String, String) = eval(
            &h,
            r#"local net = { allow = { "127.0.0.1" } }
               local a = mare.proc.run({ cmd = "curl", args = { "-s", "-f", URL }, sandbox = { network = net } })
               net.allow_local = true
               local b = mare.proc.run({ cmd = "curl", args = { "-s", "-f", URL }, sandbox = { network = net } })
               -- going around the proxy hits seatbelt itself
               local c = mare.proc.run({ cmd = "curl", args = { "-s", "-S", "--noproxy", "*", URL }, sandbox = { network = false } })
               return tostring(a.sandbox_denied), tostring(a.denial and a.denial.host), b.stdout,
                   tostring(c.sandbox_denied) .. ":" .. tostring(c.denial and c.denial.host)"#,
        )
        .await;
        assert_eq!((blocked.as_str(), host_.as_str()), ("network", "127.0.0.1"), "local addresses need allow_local");
        assert_eq!(allowed, "ok");
        assert_eq!(bypass, "network:127.0.0.1");
        h.shutdown().await;
    })
    .await
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_sandbox_blocks_a_live_loopback_listener() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let mech = mechanism(&h).await;
        if !require_backend(&h, "linux_sandbox_blocks_a_live_loopback_listener", &["bwrap", "landlock"]).await {
            return h.shutdown().await;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let baseline = std::net::TcpStream::connect(addr).expect("the local listener must be reachable");
        drop(baseline);
        let (accepted, _) = listener.accept().unwrap();
        drop(accepted);
        global(&h, "PORT", i64::from(addr.port()));
        let (code, out): (i64, String) = eval(
            &h,
            r#"local cmd = 'printf SANDBOX_STARTED; printf x > /dev/tcp/127.0.0.1/' .. PORT
               local r = mare.proc.run({ cmd = 'bash', args = { '-c', cmd },
                 sandbox = { level = 'workspace-write', network = false } })
               return r.code, (r.stdout or '') .. (r.stderr or '')"#,
        )
        .await;
        assert!(out.contains("SANDBOX_STARTED"), "{mech} did not start the command: {out}");
        assert_ne!(code, 0, "{mech} allowed the sandboxed process to connect: {out}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn missing_capabilities_refuse_unless_explicitly_accepted() {
    local(async {
        let mut env = TestEnv::new().await;
        // a backend that declares only some dimensions: no net_proxy, no fs_read
        env.set_var("MARE_SANDBOX_CAPS", "fs_write,net_block,unix_socket");
        let h = host(&env).await;
        if !require_backend(
            &h,
            "missing_capabilities_refuse_unless_explicitly_accepted",
            mare_binding::ossandbox::expect::ANY,
        )
        .await
        {
            return h.shutdown().await;
        }
        let (filtered, reads, exec_ok, exec_why): (String, String, bool, String) = eval(
            &h,
            r#"local net = { allow = { "example.com" } }
               local _, a = pcall(mare.proc.run, { cmd = "true", sandbox = { network = net } })
               local _, b = pcall(mare.proc.run, { cmd = "true", sandbox = { restrict_read = true } })
               local ok, why = mare.proc.sandbox.check("exec", "true", { network = net })
               return tostring(a), tostring(b), ok, tostring(why)"#,
        )
        .await;
        assert!(filtered.contains("SANDBOX_INSUFFICIENT") && filtered.contains("net_proxy"), "{filtered}");
        assert!(reads.contains("SANDBOX_INSUFFICIENT") && reads.contains("fs_read"), "{reads}");
        assert!(!exec_ok && exec_why.contains("net_proxy"), "the exec pre-check matches like a spawn: {exec_why}");
        // the user accepts the weaker policy explicitly: it runs, reported as partial
        let (code, enforcement, unavailable, enforced): (i64, String, String, String) = eval(
            &h,
            r#"local r = mare.proc.run({ cmd = "true",
                   sandbox = { network = { allow = { "example.com" } }, accept_partial = { "network" } } })
               return r.code, r.sandbox.enforcement, table.concat(r.sandbox.unavailable, ","),
                   table.concat(r.sandbox.enforced, ",")"#,
        )
        .await;
        assert_eq!((code, enforcement.as_str(), unavailable.as_str()), (0, "partial", "net_proxy"));
        assert!(enforced.contains("fs_write") && enforced.contains("unix_socket"), "{enforced}");
        h.shutdown().await;
    })
    .await
}

/// `MARE_SANDBOX_CAPS` only narrows the probed set — declaring a dimension the
/// backend lacks (`net_proxy` on bubblewrap without its network bridge, on Landlock) doesn't grant it.
#[tokio::test]
async fn declared_capabilities_never_widen_the_probed_set() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
        // no helper for the network bridge: bubblewrap probes no net_proxy
        env.set_var("MARE_SANDBOX_HELPER", "/nonexistent/mare");
        let h = host(&env).await;
        let m = mechanism(&h).await;
        if !require_backend(
            &h,
            "declared_capabilities_never_widen_the_probed_set",
            mare_binding::ossandbox::expect::ANY,
        )
        .await
        {
            return h.shutdown().await;
        }
        let (caps, missing, simulated, refused): (String, String, bool, String) = eval(
            &h,
            r#"local c = mare.proc.sandbox.capabilities()
               local _, e = pcall(mare.proc.run, { cmd = "true", sandbox = { accept_partial = { "spawn" } } })
               return table.concat(c.caps, ","), table.concat(c.missing, ","), c.simulated, tostring(e)"#,
        )
        .await;
        assert!(simulated);
        assert!(!caps.contains("spawn") && !missing.contains("spawn"), "caps={caps} missing={missing}");
        if m == "bwrap" || m == "landlock" {
            assert!(!caps.contains("net_proxy"), "{caps}");
        }
        // process spawning is not a sandbox dimension: `spawn` is an unknown name
        assert!(refused.contains("unknown capability"), "{refused}");
        h.shutdown().await;
    })
    .await
}

/// `mare doctor` names configured writable directories that don't exist (the permissions plugin
/// skips them as sandbox roots) as a warning, not a failure.
#[test]
fn doctor_warns_about_missing_writable_directories() {
    let home = tempfile::tempdir().unwrap();
    let ws = dunce::canonicalize(home.path()).unwrap();
    std::fs::create_dir_all(ws.join("config")).unwrap();
    std::fs::create_dir(ws.join("there")).unwrap();
    std::fs::write(
        ws.join("config/init.lua"),
        r#"mare.plugin("permissions", { writable = { "there", "gone" },
               presets = { build = { like = "auto", writable = { "also-gone" } } } })"#,
    )
    .unwrap();
    let out = mare_cmd()
        .args(["doctor", "--json"])
        .env("MARE_HOME", &ws)
        .current_dir(&ws)
        .stdin(std::process::Stdio::null())
        .run_output();
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let checks = doc["checks"].as_array().unwrap();
    let line = checks.iter().find(|c| c["name"] == "sandbox writable").unwrap_or_else(|| panic!("{doc:#}"));
    let detail = line["detail"].as_str().unwrap();
    assert_eq!(line["warn"], true, "{line}");
    assert!(detail.contains(&ws.join("gone").display().to_string()), "{detail}");
    assert!(detail.contains(&ws.join("also-gone").display().to_string()), "{detail}");
    assert!(!detail.contains(&ws.join("there").display().to_string()), "{detail}");
}

#[tokio::test]
async fn unix_sockets_follow_one_list_in_policy_and_kernel() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let dir = tempfile::tempdir().unwrap();
        let sock = dunce::canonicalize(dir.path()).unwrap().join("d.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                drop(s);
            }
        });
        global(&h, "SOCK", sock.to_string_lossy().to_string());
        // policy layer: a restricted network reaches only the listed sockets
        let (unlisted, listed, open): (bool, bool, bool) = eval(
            &h,
            r#"local S = mare.proc.sandbox
               return S.check("network", "unix:" .. SOCK, { network = false }),
                   S.check("network", "unix:" .. SOCK, { network = false, unix_sockets = { SOCK } }),
                   S.check("network", "unix:" .. SOCK, { network = true })"#,
        )
        .await;
        assert!(!unlisted && listed && open);
        // kernel: the same list — an unlisted socket is unreachable, a listed one works (seatbelt);
        // Linux backends' seccomp filter blocks every AF_UNIX socket (stricter than the list)
        let mech = mechanism(&h).await;
        if mech == "bwrap" || mech.starts_with("landlock") {
            let blocked: String = eval(
                &h,
                r#"local py = "import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); print('ok')"
                   local a = mare.proc.run({ cmd = "python3", args = { "-c", py, SOCK }, sandbox = { network = false } })
                   return a.stdout .. "|" .. tostring(a.code)"#,
            )
            .await;
            assert!(!blocked.starts_with("ok"), "an unlisted unix socket was reachable: {blocked}");
        }
        if mech == "seatbelt" {
            let (blocked, allowed): (String, String) = eval(
                &h,
                r#"local py = "import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); print('ok')"
                   local a = mare.proc.run({ cmd = "python3", args = { "-c", py, SOCK }, sandbox = { network = false } })
                   local b = mare.proc.run({ cmd = "python3", args = { "-c", py, SOCK },
                       sandbox = { network = false, unix_sockets = { SOCK } } })
                   return a.stdout .. "|" .. tostring(a.code), b.stdout"#,
            )
            .await;
            assert!(!blocked.starts_with("ok"), "an unlisted unix socket was reachable: {blocked}");
            assert_eq!(allowed.trim(), "ok");
        }
        h.shutdown().await;
    })
    .await
}
