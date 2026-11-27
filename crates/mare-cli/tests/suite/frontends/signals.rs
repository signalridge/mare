//! Real process regressions for signals.
#[cfg(unix)]
use std::time::Duration;

use super::*;

#[cfg(unix)]
#[test]
fn headless_sigint_cancels_the_turn_instead_of_hanging() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([{ "tool_calls": [{ "name": "bash", "arguments": { "command": "touch started; sleep 30" } }] }, { "text": "done" }])
            .to_string(),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["-p", "--model", "faux/echo", "--no-session", "--yolo", "run it"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let child = ChildGroup::spawn(&mut cmd).unwrap();
    // signal only once the tool is running (startup of a debug build can be slow)
    let t0 = std::time::Instant::now();
    while !home.path().join("started").exists() {
        assert!(t0.elapsed() < std::time::Duration::from_secs(30), "the command never started");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let out = wait_output_timeout(child, Duration::from_secs(15));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cancelling the turn"), "status {:?}: {err}", out.status);
    assert_eq!(out.status.code(), Some(130), "SIGINT exits 130: {err}");
}

/// The first SIGTERM cancels the turn and completes the regular app/shutdown path.
#[cfg(unix)]
#[test]
fn headless_sigterm_runs_shutdown_hooks_before_exit() {
    use std::time::{Duration, Instant};

    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    let started = home.path().join("started");
    let shutdown_started = home.path().join("shutdown-started");
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        &script,
        json!([{ "tool_calls": [{ "name": "bash", "arguments": { "command": format!("touch {:?}; sleep 30", started) } }] }, { "text": "done" }])
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        config.join("init.lua"),
        format!(
            r#"
mare.on("app/shutdown", function()
  local marker = assert(io.open({shutdown_started:?}, "w"))
  marker:write("finished")
  marker:close()
end)
"#
        ),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["-p", "--model", "faux/echo", "--no-session", "--yolo", "run it"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();

    let startup_deadline = Instant::now() + Duration::from_secs(30);
    while !started.exists() {
        if Instant::now() >= startup_deadline {
            drop(child);
            panic!("the scripted command never started");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.0.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < exit_deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                drop(child);
                panic!("first SIGTERM did not finish the regular shutdown path");
            }
        }
    };
    assert!(shutdown_started.exists(), "app/shutdown must finish before the normal exit");
    assert_eq!(status.code(), Some(1), "SIGTERM keeps the existing headless error status");
}

/// A second signal must force-exit even when an app/shutdown hook blocks on the stdout pipe.
#[cfg(unix)]
#[test]
fn second_signal_forces_exit_while_shutdown_output_is_blocked() {
    use std::time::{Duration, Instant};

    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    let started = home.path().join("started");
    let shutdown_started = home.path().join("shutdown-started");
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        &script,
        json!([{ "tool_calls": [{ "name": "bash", "arguments": { "command": format!("touch {:?}; sleep 30", started) } }] }, { "text": "done" }])
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        config.join("init.lua"),
        format!(
            r#"
mare.on("app/shutdown", function()
  local marker = assert(io.open({shutdown_started:?}, "w"))
  marker:write("started")
  marker:close()
  io.stdout:setvbuf("no")
  io.stdout:write(string.rep("x", 16 * 1024 * 1024))
  io.stdout:flush()
end)
"#
        ),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["-p", "--model", "faux/echo", "--no-session", "--yolo", "run it"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped()) // Keep the pipe unread; app/shutdown fills it and blocks.
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();

    let startup_deadline = Instant::now() + Duration::from_secs(30);
    while !started.exists() {
        if Instant::now() >= startup_deadline {
            drop(child);
            panic!("the scripted command never started");
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // SIGTERM begins the ordinary drain; the shutdown hook marks entry before its blocking write.
    // SIGINT during that write must bypass Tokio and exit directly with the interrupt code.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let drain_deadline = Instant::now() + Duration::from_secs(30);
    while !shutdown_started.exists() {
        if Instant::now() >= drain_deadline {
            drop(child);
            panic!("SIGTERM did not enter app/shutdown");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    std::thread::sleep(Duration::from_millis(100));
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };

    let force_deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.0.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < force_deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                drop(child);
                panic!("second signal did not bypass blocked shutdown output");
            }
        }
    };
    assert_eq!(status.code(), Some(130), "second SIGINT should force exit with 130");
}

/// Windows has no Unix signal equivalent here: a console break starts shutdown, whose process
/// deadline must still terminate the child while its synchronous stdout write is blocked.
#[cfg(windows)]
#[test]
fn windows_shutdown_deadline_forces_exit_while_output_is_blocked() {
    use std::io::BufRead;
    use std::os::windows::process::CommandExt;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    let shutdown_started = home.path().join("shutdown-started");
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(&script, json!([{ "text": "done", "delay_ms": 30_000 }]).to_string()).unwrap();
    std::fs::write(
        config.join("init.lua"),
        format!(
            r#"
mare.on("app/shutdown", function()
  local marker = assert(io.open({shutdown_started:?}, "w"))
  marker:write("started")
  marker:close()
  io.stdout:setvbuf("no")
  io.stdout:write(string.rep("x", 16 * 1024 * 1024))
  io.stdout:flush()
end)
"#
        ),
    )
    .unwrap();

    let mut child_command = mare_cmd();
    child_command
        .args(["--json", "--model", "faux/echo", "--trust", "--no-session", "--yolo", "run it"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped()) // Leave the pipe unread so app/shutdown blocks writing to it.
        .stderr(Stdio::piped()) // --json routes Lua io.stdout to stderr; keep stderr piped & unread.
        .creation_flags(CREATE_NEW_PROCESS_GROUP);
    let mut child = child_command.spawn().unwrap();

    let _stderr = child.stderr.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (line_tx, line_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = BufReader::new(stdout);
        let mut first = String::new();
        let read = out.read_line(&mut first);
        let _ = line_tx.send((read, first, out));
    });
    let (read, first, _out) = match line_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(value) => value,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("headless did not start: {error}");
        }
    };
    if let Err(error) = read {
        let _ = child.kill();
        let _ = child.wait();
        panic!("could not read headless startup event: {error}");
    }
    if !first.contains("\"session\"") {
        let _ = child.kill();
        let _ = child.wait();
        panic!("headless did not start: {first}");
    }
    // The JSON session record is written just before headless installs its persistent signal listeners.
    std::thread::sleep(Duration::from_millis(500));

    let signal_sent_at = Instant::now();
    // The test child owns a new console process group, so the event targets only that process.
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) } == 0 {
        let _ = child.kill();
        let _ = child.wait();
        panic!("could not send the first console break");
    }
    let drain_deadline = Instant::now() + Duration::from_secs(10);
    while !shutdown_started.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("the child exited before app/shutdown blocked output: {status}");
        }
        if Instant::now() >= drain_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("console break did not enter app/shutdown");
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    std::thread::sleep(Duration::from_millis(250));
    assert!(child.try_wait().unwrap().is_none(), "the child should remain in its bounded drain");
    let force_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < force_deadline => std::thread::sleep(Duration::from_millis(25)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the five-second deadline did not bypass blocked shutdown output");
            }
        }
    };
    let elapsed = signal_sent_at.elapsed();
    assert!(
        elapsed >= Duration::from_millis(4_500),
        "the regular drain must get the five-second window before force exit; elapsed {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(10), "the process exceeded the five-second bound: {elapsed:?}");
    assert_eq!(status.code(), Some(1), "the deadline force exit uses the first signal's exit code");
    drop(_stderr);
}

/// Observe real kernel pipe backpressure before sending any signal. The reader stays open
/// throughout: signal escalation, rather than an EPIPE, must release the blocked process.
#[cfg(target_os = "linux")]
fn signal_after_stdout_is_already_blocked(second: bool) {
    use std::io::BufRead;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::time::{Duration, Instant};
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "done" }]).to_string()).unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("fill-output.lua"),
        r#"return {
        name = "fill-output", apply = function(ctx)
            ctx:on("turn/started", function()
                -- Native fallback remains independent of the public-event async writer.
                io.write(string.rep("blocked-output", 32768))
                io.flush()
            end)
        end,
    }"#,
    )
    .unwrap();
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // Retain a writer solely to inspect kernel POLLOUT readiness; no bytes are written by it.
    let reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    let mut cmd = mare_cmd();
    cmd.args(["-p", "--json", "--model", "faux/echo", "--no-session", "--yolo", "go"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(writer.try_clone().unwrap());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["type"], "session");
    let fd = reader.as_raw_fd();
    let capacity = unsafe { libc::fcntl(fd, libc::F_GETPIPE_SZ) };
    assert!(capacity > 0);
    let ready = Instant::now() + Duration::from_secs(20);
    loop {
        let mut buffered: libc::c_int = 0;
        assert_eq!(unsafe { libc::ioctl(fd, libc::FIONREAD, &mut buffered) }, 0);
        // The Lua write is routed to stderr while stdout stays clean JSONL. Once the stderr kernel
        // pipe is full, Lua's synchronous io.write cannot return until this reader drains.
        let mut writable = libc::pollfd { fd: writer.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
        assert!(unsafe { libc::poll(&mut writable, 1, 0) } >= 0);
        if buffered > 0 && writable.revents & libc::POLLOUT == 0 {
            break;
        }
        if Instant::now() >= ready || child.0.try_wait().unwrap().is_some() {
            drop(child);
            panic!("headless did not fill the stderr pipe: {buffered}/{capacity}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let sent = Instant::now();
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    if second {
        // The first callback must have run before a second signal can be delivered; the
        // native signal counter is set synchronously in that callback, even while Lua is stuck.
        // A different signal avoids POSIX coalescing two identical pending standard signals.
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
    }
    let limit = if second { Duration::from_secs(2) } else { Duration::from_secs(7) };
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        if sent.elapsed() >= limit {
            drop(child);
            panic!("signal did not terminate a headless process whose stderr pipe was already blocked");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(matches!(status.code(), Some(130 | 143)), "native signal exit: {status}");
    if !second {
        assert_eq!(status.code(), Some(130), "the first SIGINT deadline preserves exit code130");
        assert!(sent.elapsed() >= Duration::from_secs(4), "first signal should allow the graceful deadline");
    }
    drop(writer);
    drop(reader);
    drop(output);
}

#[cfg(target_os = "linux")]
#[test]
fn first_signal_starts_shutdown_deadline_when_stdout_is_already_blocked() {
    signal_after_stdout_is_already_blocked(false);
}

#[cfg(target_os = "linux")]
#[test]
fn second_signal_exits_when_stdout_was_blocked_before_the_first_signal() {
    signal_after_stdout_is_already_blocked(true);
}

/// A graceful RPC signal path requests code zero. If its shutdown hook blocks the
/// runtime, the native deadline must mark the forced/truncated exit as a failure.
#[cfg(unix)]
#[test]
fn rpc_forced_shutdown_deadline_cannot_report_success() {
    use std::os::fd::FromRawFd;
    use std::time::{Duration, Instant};
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    let started = home.path().join("shutdown-started");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("block-shutdown.lua"),
        format!(
            r#"return {{
        name = "block-shutdown", apply = function(ctx)
            ctx:on("app/shutdown", function()
                local file = assert(io.open({started:?}, "w"))
                file:write("started"); file:close()
                io.write(string.rep("blocked shutdown output", 1024 * 1024))
                io.flush()
            end)
        end,
    }}"#
        ),
    )
    .unwrap();
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let _stderr_reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let stderr_writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session", "--yolo"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr_writer);
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut rpc = RpcClient::new(BufReader::new(child.0.stdout.take().unwrap()), child.0.stdin.take().unwrap());
    let ready =
        rpc.call_raw("initialize", json!({"protocol_version": mare_protocol::PROTOCOL_VERSION}), |_| json!(null));
    assert_eq!(ready["result"]["protocol_version"], mare_protocol::PROTOCOL_VERSION, "{ready}");
    let signaled = Instant::now();
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) }, 0);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        if signaled.elapsed() >= Duration::from_secs(7) {
            drop(child);
            panic!("RPC shutdown exceeded the native five-second deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Hold both transport handles through process exit; neither EOF nor EPIPE may be
    // responsible for releasing the shutdown hook's blocked write.
    assert!(started.exists(), "RPC must enter the graceful shutdown hook before forcing exit");
    assert!(signaled.elapsed() >= Duration::from_secs(4), "must exercise the native deadline");
    assert_eq!(status.code(), Some(1), "forced zero-code drain must fail: {status}");
    drop(rpc);
}

/// TERM=dumb still runs an interactive session: SIGINT during work cancels that turn,
/// then a later request must remain usable after the process shutdown grace has elapsed.
#[cfg(unix)]
#[test]
fn plain_turn_interrupt_does_not_leave_a_process_exit_deadline() {
    let body = format!("MODEL-FIRST-BEGAN\n\n{}", "interrupt-me ".repeat(1000));
    let script = json!([
        { "text": body, "delay_ms": 25 },
        { "text": "SECOND-REQUEST-SUCCEEDED" }
    ]);
    let cmd = MareCmd::new()
        .no_init()
        .no_term_probe()
        .args(["--model", "faux/echo", "--no-session", "--yolo", "--trust"])
        .env("TERM", "dumb")
        .faux(script);
    let entered = cmd.home_path().join("provider-entered");
    let entered_lit = serde_json::to_string(&entered.to_string_lossy().as_ref()).unwrap();
    let plugin = format!(
        "return {{ name='mark-first-stream', apply=function(ctx) local marked=false; \
         ctx:on('item/delta', function() if not marked then marked=true; \
         local f=assert(io.open({entered_lit},'w')); f:write('entered'); f:close() \
         end end) end }}"
    );
    let mut p = cmd.plugin("mark-first-stream", &plugin).pty(100, 30);

    p.wait_text("initial prompt", |s| s.contains("> "));
    p.send(b"first request\n");
    p.wait_text("Ctrl+C interrupts", |s| s.contains("Ctrl+C interrupts"));
    let deadline = std::time::Instant::now() + Duration::from_secs(WAIT_SECS);
    while !entered.exists() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for first provider stream");
        std::thread::sleep(Duration::from_millis(20)); // sleep-ok: poll until first provider stream marker exists
    }

    assert_eq!(unsafe { libc::kill(p.pid() as i32, libc::SIGINT) }, 0);
    p.wait_text("second prompt", |s| s.matches("> ").count() >= 2);
    std::thread::sleep(Duration::from_secs(6)); // sleep-ok: intentionally cross the old 5s exit deadline
    assert!(p.running(), "turn cancellation killed plain mode");

    p.send(b"second request\n");
    p.wait_text("SECOND-REQUEST-SUCCEEDED", |s| s.contains("SECOND-REQUEST-SUCCEEDED"));
    p.send(b"/exit\n");
    assert!(p.wait_exit(10), "mare exited with non-zero status");
}

/// SIGHUP runs shutdown hooks before exiting cleanly.
#[cfg(unix)]
#[test]
fn headless_sighup_runs_shutdown_hooks_before_exit() {
    use std::time::Instant;

    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    let started = home.path().join("started");
    let shutdown_done = home.path().join("shutdown-hup");
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        &script,
        json!([{ "tool_calls": [{ "name": "bash", "arguments": { "command": format!("touch {:?}; sleep 30", started) } }] }, { "text": "done" }])
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        config.join("init.lua"),
        format!(
            r#"
mare.on("app/shutdown", function()
  local marker = assert(io.open({shutdown_done:?}, "w"))
  marker:write("hup-finished")
  marker:close()
end)
"#
        ),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["-p", "--model", "faux/echo", "--no-session", "--yolo", "run it"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();

    let startup_deadline = Instant::now() + Duration::from_secs(30);
    while !started.exists() {
        if Instant::now() >= startup_deadline {
            drop(child);
            panic!("the scripted command never started");
        }
        std::thread::sleep(Duration::from_millis(25)); // sleep-ok: sync test polls startup marker file before sending SIGHUP
    }
    unsafe { libc::kill(child.id() as i32, libc::SIGHUP) };

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let _status = loop {
        match child.0.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < exit_deadline => std::thread::sleep(Duration::from_millis(20)), // sleep-ok: sync test polls child exit after SIGHUP
            None => {
                drop(child);
                panic!("SIGHUP did not finish the regular shutdown path");
            }
        }
    };
    assert!(shutdown_done.exists(), "app/shutdown must finish on SIGHUP before exit");
}

/// `mare acp` must catch SIGINT and run `app/shutdown` promptly instead of hanging for the
/// 5-second `_exit(130)` force-exit watchdog.
#[cfg(unix)]
#[test]
fn acp_sigint_runs_shutdown_hooks_promptly() {
    use std::io::{BufRead, BufReader, Write};
    use std::time::Instant;

    let home = tempfile::tempdir().unwrap();
    let shutdown_done = home.path().join("acp-shutdown-int");
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        format!(
            r#"
mare.on("app/shutdown", function()
  local marker = assert(io.open({shutdown_done:?}, "w"))
  marker:write("int-finished")
  marker:close()
end)
"#
        ),
    )
    .unwrap();

    let cwd = dunce::canonicalize(home.path()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "--no-session", "--yolo", "--trust", "acp"])
        .env("MARE_HOME", home.path())
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut stdin = child.0.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}})
    )
    .unwrap();
    stdin.flush().unwrap();

    // Wait for `initialize` before sending `session/new` so we know `acp::run()` and signal
    // handlers are live (not still inside pre-run `init.lua` flag discovery).
    let mut line = String::new();
    let mut init_ready = false;
    while stdout.read_line(&mut line).unwrap_or(0) > 0 {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
            && v["id"] == 1
        {
            assert!(v["error"].is_null(), "initialize failed: {v}");
            init_ready = true;
            break;
        }
        line.clear();
    }
    assert!(init_ready, "mare acp never replied to initialize");

    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}}))
        .unwrap();
    stdin.flush().unwrap();

    line.clear();
    let mut session_ready = false;
    while stdout.read_line(&mut line).unwrap_or(0) > 0 {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
            && v["id"] == 2
        {
            assert!(v["error"].is_null(), "session/new failed: {v}");
            session_ready = true;
            break;
        }
        line.clear();
    }
    assert!(session_ready, "mare acp never replied to session/new");

    let signaled = Instant::now();
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.0.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < exit_deadline => std::thread::sleep(Duration::from_millis(20)), // sleep-ok: sync test polls child exit after SIGINT
            None => {
                drop(child);
                panic!("mare acp did not exit after SIGINT");
            }
        }
    };
    drop(stdin);
    assert!(
        shutdown_done.exists(),
        "mare acp must run app/shutdown on SIGINT (not wait for 5s _exit(130)); elapsed={:?} status={status}",
        signaled.elapsed()
    );
    assert!(
        signaled.elapsed() < Duration::from_secs(4),
        "mare acp SIGINT shutdown took too long ({:?}), may have hit 5s forced exit",
        signaled.elapsed()
    );
    assert_eq!(status.code(), Some(130), "expected SIGINT exit code 130: {status}");
}
