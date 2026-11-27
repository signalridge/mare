use super::*;

/// Primary RPC `prompt` (`source = "rpc"`) is user input and must reset `jobs.max_wakeups`.
#[test]
fn primary_rpc_prompt_resets_jobs_max_wakeups() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("init.lua"), "mare.setup { plugins = { jobs = { max_wakeups = 1, batch_ms = 50 } } }\n")
        .unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(
        &faux,
        json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "description": "bg1", "command": "sleep 0.2", "run_in_background": true } }] },
            { "text": "started-1" },
            { "text": "woke-1" },
            { "tool_calls": [{ "name": "bash", "arguments": { "description": "bg2", "command": "sleep 0.2", "run_in_background": true } }] },
            { "text": "started-2" },
            { "text": "woke-2" }
        ])
        .to_string(),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut c = RpcClient::timed(BufReader::new(child.0.stdout.take().unwrap()), child.0.stdin.take().unwrap());
    let init = c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    c.call_raw("events.subscribe", json!({ "include": ["turn/completed", "host/notice", "turn/started"] }), |_| {
        json!(null)
    });

    let p1 = c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "start 1" }), |_| json!(null));
    assert!(p1.get("error").is_none(), "{p1}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": p1["result"]["turn_id"] }), |_| json!(null));
    wait_end(&mut c, &mut child.0, "woke-1", std::time::Duration::from_secs(10)).unwrap_or_else(|e| panic!("{e}"));

    // Exhausted max_wakeups=1; a second primary RPC prompt must reset the counter.
    let p2 = c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "start 2" }), |_| json!(null));
    assert!(p2.get("error").is_none(), "{p2}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": p2["result"]["turn_id"] }), |_| json!(null));
    wait_end(&mut c, &mut child.0, "woke-2", std::time::Duration::from_secs(10)).unwrap_or_else(|e| panic!("{e}"));

    c.call_raw("shutdown", json!({}), |_| json!(null));
    let _ = child.wait_timeout(std::time::Duration::from_secs(30));
}

fn read_stderr_bounded(mut err: std::process::ChildStderr, timeout: std::time::Duration) -> String {
    use std::io::Read as _;
    const MAX_BYTES: usize = 64 * 1024;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match err.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let deadline = std::time::Instant::now() + timeout;
    let mut out = Vec::new();
    let mut timed_out = false;
    loop {
        let rem = deadline.saturating_duration_since(std::time::Instant::now());
        if rem.is_zero() {
            timed_out = true;
            break;
        }
        match rx.recv_timeout(rem) {
            Ok(chunk) => {
                let avail = MAX_BYTES.saturating_sub(out.len());
                out.extend_from_slice(&chunk[..chunk.len().min(avail)]);
                if out.len() >= MAX_BYTES {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                timed_out = true;
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    const TIMEOUT_MARKER: &str = "\n[stderr read timed out]";
    let mut s = String::from_utf8_lossy(&out).into_owned();
    let max_body = if timed_out { MAX_BYTES.saturating_sub(TIMEOUT_MARKER.len()) } else { MAX_BYTES };
    if s.len() > max_body {
        s.truncate(s.floor_char_boundary(max_body));
    }
    if timed_out {
        s.push_str(TIMEOUT_MARKER);
    }
    s
}

fn kill_and_collect_diagnostics(child: &mut std::process::Child) -> String {
    // Kill the whole process group (when isolated) before reaping / reading stderr so a
    // descendant holding the stderr write end cannot block diagnostics indefinitely.
    #[cfg(unix)]
    {
        let pid = child.id();
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        if pgid == pid as libc::pid_t {
            mare_binding::platform::signal_tree(Some(pid), mare_binding::platform::Signal::Kill);
        }
    }
    #[cfg(not(unix))]
    mare_binding::platform::signal_tree(Some(child.id()), mare_binding::platform::Signal::Kill);
    let _ = child.kill();
    let status = child.wait().map(|s| s.to_string()).unwrap_or_else(|e| format!("wait err: {e}"));
    let stderr = child
        .stderr
        .take()
        .map(|err| read_stderr_bounded(err, std::time::Duration::from_millis(500)))
        .unwrap_or_default();
    format!("status={status}; stderr={stderr:?}")
}

fn wait_end(
    c: &mut RpcClient,
    child: &mut std::process::Child,
    expected: &str,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match c.read_deadline(deadline) {
            Ok(m) => {
                if m["method"] == "event" {
                    let ty = m["params"]["type"].as_str().unwrap_or("");
                    if ty == "turn/completed" && m["params"]["data"]["text"] == expected {
                        if !m["params"]["data"]["error"].is_null() {
                            let diag = kill_and_collect_diagnostics(child);
                            return Err(format!("unexpected turn/completed error: {m}; {diag}"));
                        }
                        return Ok(());
                    }
                    seen.push(m);
                }
            }
            Err(reason) => {
                let diag = kill_and_collect_diagnostics(child);
                return Err(format!(
                    "timed out waiting for turn/completed `{expected}` ({reason}); {diag}; seen: {seen:?}"
                ));
            }
        }
    }
}

/// Regression test (scenario inspired by Codex `codex-rs/utils/pty/src/child_tests.rs:18-42`):
/// when a child process hangs without writing EOF, `wait_end` must return by the deadline,
/// kill the child first, and then collect stderr diagnostics without blocking on child exit or EOF.
#[cfg(unix)]
#[test]
fn wait_end_times_out_and_kills_hung_child_before_collecting_diagnostics() {
    let mut cmd = std::process::Command::new("sh");
    cmd.args(["-c", "echo hung-diag >&2; exec sleep 60"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let pid = child.id();
    let mut c = RpcClient::timed(BufReader::new(child.0.stdout.take().unwrap()), child.0.stdin.take().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let res = wait_end(&mut c, &mut child.0, "woke-1", std::time::Duration::from_millis(200));
        let exited = child.0.try_wait().map(|s| s.is_some()).unwrap_or(false);
        drop(child);
        let _ = tx.send((res, exited));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok((res, exited)) => {
            let err = res.expect_err("expected timeout failure, got Ok");
            assert!(exited, "child {pid} should already be killed and reaped on timeout: {err}");
            assert!(err.contains("hung-diag"), "expected stderr diagnostics in error: {err}");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("wait_end hung past 2s watchdog for pid {pid} (timeout=200ms did not fire)");
        }
        Err(e) => {
            panic!("watchdog recv error: {e}");
        }
    }
}

/// Regression test: when a hung child has a descendant holding `stderr` open
/// (`sleep 60 &`), `kill_and_collect_diagnostics` must kill the process group and bound stderr
/// reads so `wait_end` still returns by the deadline with `hung-diag` diagnostics, and the
/// descendant itself is dead/reaped without hitting the stderr read timeout fallback.
#[cfg(unix)]
#[test]
fn wait_end_kills_descendant_holding_stderr_and_returns_diagnostics_within_deadline() {
    use std::os::unix::process::CommandExt as _;
    struct PgGuard(u32);
    impl Drop for PgGuard {
        fn drop(&mut self) {
            mare_binding::platform::signal_tree(Some(self.0), mare_binding::platform::Signal::Kill);
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("descendant.pid");
    let mut cmd = std::process::Command::new("sh");
    cmd.args([
        "-c",
        "echo hung-diag >&2; sleep 60 & echo $! > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"; exec sleep 60",
        "sh",
        pid_file.to_str().unwrap(),
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .process_group(0);
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let pid = child.id();
    let guard = PgGuard(pid);
    let desc_pid: libc::pid_t = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_file)
                && let Ok(parsed) = contents.trim().parse::<libc::pid_t>()
            {
                break parsed;
            }
            assert!(std::time::Instant::now() < deadline, "timed out waiting for descendant pid file");
            // sleep-ok: poll interval on an external process state (pid file / /proc); bounded by the deadline above
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    let mut c = RpcClient::timed(BufReader::new(child.0.stdout.take().unwrap()), child.0.stdin.take().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let res = wait_end(&mut c, &mut child.0, "woke-1", std::time::Duration::from_millis(200));
        let exited = child.0.try_wait().map(|s| s.is_some()).unwrap_or(false);
        drop(child);
        let _ = tx.send((res, exited));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok((res, exited)) => {
            let err = res.expect_err("expected timeout failure, got Ok");
            assert!(exited, "child {pid} should already be killed and reaped on timeout: {err}");
            assert!(err.contains("hung-diag"), "expected stderr diagnostics in error: {err}");
            assert!(
                !err.contains("[stderr read timed out]"),
                "stderr diagnostics should reach EOF without hitting the read timeout fallback: {err}"
            );
            // Verify before PgGuard drops that the descendant itself has stopped (reaped or zombie).
            let reap_deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            loop {
                if pid_stopped(desc_pid) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < reap_deadline,
                    "descendant {desc_pid} was still running after wait_end timeout (before PgGuard cleanup)"
                );
                // sleep-ok: poll interval on an external process state (pid file / /proc); bounded by the deadline above
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            drop(guard);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("wait_end hung past 2s watchdog for pid {pid} with descendant holding stderr");
        }
        Err(e) => {
            panic!("watchdog recv error: {e}");
        }
    }
}

/// Separate regression test verifying `read_stderr_bounded` enforces both its timeout fallback
/// (`[stderr read timed out]`) when the write end remains open and its 64 KiB byte cap.
#[cfg(unix)]
#[test]
fn read_stderr_bounded_enforces_timeout_and_byte_cap() {
    let mut sleeping = std::process::Command::new("sh")
        .args(["-c", "echo partial-diag >&2; exec sleep 60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let err = sleeping.stderr.take().unwrap();
    let start = std::time::Instant::now();
    let bounded = read_stderr_bounded(err, std::time::Duration::from_millis(100));
    let elapsed = start.elapsed();
    let _ = sleeping.kill();
    let _ = sleeping.wait();
    assert!(bounded.contains("partial-diag"), "expected partial stderr output: {bounded:?}");
    assert!(bounded.contains("[stderr read timed out]"), "expected timeout marker in bounded stderr: {bounded:?}");
    assert!(elapsed < std::time::Duration::from_secs(1), "bounded read took too long: {elapsed:?}");

    let mut noisy = std::process::Command::new("sh")
        .args(["-c", "head -c 80000 /dev/zero | tr '\\0' 'a' >&2"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let capped = read_stderr_bounded(noisy.stderr.take().unwrap(), std::time::Duration::from_secs(2));
    let _ = noisy.wait();
    assert_eq!(capped.len(), 64 * 1024, "expected 64 KiB cap, got {} bytes", capped.len());
    assert!(!capped.contains("[stderr read timed out]"), "byte-capped read should stop without timeout marker");
}

#[cfg(unix)]
#[test]
fn read_stderr_bounded_caps_non_utf8_and_multibyte_boundaries_within_64_kib() {
    let mut non_utf8 = std::process::Command::new("sh")
        .args(["-c", "LC_ALL=C head -c 80000 /dev/zero | LC_ALL=C tr '\\0' '\\377' >&2"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let capped_non_utf8 = read_stderr_bounded(non_utf8.stderr.take().unwrap(), std::time::Duration::from_secs(2));
    let _ = non_utf8.wait();
    assert!(
        capped_non_utf8.len() <= 64 * 1024,
        "expected non-UTF-8 stderr to stay within 64 KiB, got {} bytes",
        capped_non_utf8.len()
    );
    assert!(!capped_non_utf8.is_empty(), "expected non-empty lossy UTF-8 output");
    assert!(
        !capped_non_utf8.contains("[stderr read timed out]"),
        "byte-capped non-UTF-8 read should stop without timeout marker"
    );

    // 65,534 ASCII bytes + 4-byte emoji ('😀' = F0 9F 98 80) crosses the 64 KiB raw-byte cap
    // mid-codepoint; lossy decoding must not push the returned String past 64 KiB.
    let mut split_utf8 = std::process::Command::new("sh")
        .args(["-c", "LC_ALL=C printf '%*s\\360\\237\\230\\200b' 65534 '' | LC_ALL=C tr ' ' 'a' >&2"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let capped_split = read_stderr_bounded(split_utf8.stderr.take().unwrap(), std::time::Duration::from_secs(2));
    let _ = split_utf8.wait();
    assert!(
        capped_split.len() <= 64 * 1024,
        "expected split multi-byte stderr to stay within 64 KiB, got {} bytes",
        capped_split.len()
    );
    assert_eq!(capped_split.len(), 65534);
    assert!(!capped_split.contains('\u{FFFD}'), "partial trailing codepoint should be dropped");
}

#[cfg(unix)]
fn pid_stopped(pid: libc::pid_t) -> bool {
    !pid_alive(pid as u32)
}

#[cfg(unix)]
#[test]
fn pid_stopped_treats_unreaped_zombie_as_stopped() {
    let mut running = std::process::Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let running_pid = running.id() as libc::pid_t;
    assert!(!pid_stopped(running_pid), "running process {running_pid} must not be treated as stopped");
    let _ = running.kill();
    let _ = running.wait();
    assert!(pid_stopped(running_pid), "killed and reaped process {running_pid} must be treated as stopped");

    let mut zombie_child = std::process::Command::new("sh")
        .args(["-c", "exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let zombie_pid = zombie_child.id() as libc::pid_t;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !pid_stopped(zombie_pid) {
        if std::time::Instant::now() >= deadline {
            let _ = zombie_child.wait();
            panic!("unreaped zombie {zombie_pid} was not treated as stopped");
        }
        // sleep-ok: poll interval on an external process state (pid file / /proc); bounded by the deadline above
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(unsafe { libc::kill(zombie_pid, 0) }, 0, "unreaped zombie {zombie_pid} still has a PID entry");
    let _ = zombie_child.wait();
    assert!(pid_stopped(zombie_pid), "reaped zombie {zombie_pid} must still be treated as stopped");
}
