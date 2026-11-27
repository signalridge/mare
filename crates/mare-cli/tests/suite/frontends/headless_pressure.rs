//! Real stdout backpressure: reader stays open, with kernel readiness as the barrier.
use crate::common::*;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::{Duration, Instant};

pub(super) struct Pressure {
    pub(super) child: ChildGroup,
    output: BufReader<std::fs::File>,
    writer: std::fs::File,
    flood_listener: Option<std::net::TcpListener>,
    pub(super) _home: tempfile::TempDir,
}

impl Pressure {
    fn new(timeout: bool) -> Self {
        Self::configured(timeout, |_| String::new())
    }

    pub(super) fn configured(timeout: bool, configure: impl FnOnce(&std::path::Path) -> String) -> Self {
        let home = tempfile::tempdir().unwrap();
        let script = home.path().join("faux.json");
        std::fs::write(&script, json!([{ "text":"unused" }]).to_string()).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let flood_url = format!("http://127.0.0.1:{}/flood", listener.local_addr().unwrap().port());
        let config = home.path().join("config");
        std::fs::create_dir_all(config.join("plugins")).unwrap();
        std::fs::write(config.join("init.lua"), "mare.setup { plugins = { agent = { enabled = false }, status = { enabled = false }, jobs = { enabled = false } } }").unwrap();
        let prepare = configure(home.path());
        let notice_count = if timeout { "16" } else { "6000" };
        std::fs::write(config.join("plugins/pressure.lua"), r#"
            return { name = "pressure", provide = { "agent" }, inject = { "bus", "task" }, apply = function(ctx)
                local done_tx, done_rx = mare.task.channel()
                local active_turn, finished_res
                local svc = {}
                for _, m in ipairs({
                    "prompt", "steer", "cancel", "continue", "wait", "get", "list_queue",
                    "update_queued", "delete_queued", "list_messages", "messages",
                    "prepare_commit", "activity", "register_activity", "set_messages",
                    "adopt_session", "session_revision", "check_session_revision", "set_tool_states",
                    "set_pending_request", "pending_request", "append", "persist_error", "notify",
                    "interrupted", "set_session_id", "set_queued", "baseline_cut",
                }) do
                    svc[m] = function() return {} end
                end
                svc.get = function() return { busy = active_turn ~= nil, queued = 0 } end
                svc.activity = function() return { version = 1, busy = active_turn ~= nil, queued = 0, turn_cancelled = false } end
                svc.interrupted = function() return false end
                svc.cancel = function()
                    local had = active_turn
                    if not finished_res then
                        finished_res = { turn_id = active_turn or "pressure-turn", status = "cancelled", termination_reason = "aborted", error = "aborted", started_at_ms = 0 }
                        done_tx:send(finished_res)
                    end
                    return { turn_id = had, removed = mare.json.array() }
                end
                svc.wait = function()
                    return finished_res or done_rx:recv()
                end
                svc.prompt = function(_, _, text, opts)
                    local tid = (opts and opts.turn_id) or "pressure-turn"
                    active_turn = tid
                    finished_res = nil
                    __PREPARE__
                    ctx:spawn(function()
                        ctx.bus:emit("turn/started", {turn_id=tid,trigger="prompt",text=text})
                        ctx.bus:emit("host/notice", {level="info",text="pressure-block:" .. string.rep("x",524288)})
                        for i=1,__NOTICE_COUNT__ do ctx.bus:emit("host/notice", {level="info",text="pressure:" .. i}) end
                        mare.http.request({ url = __FLOOD_BARRIER__, timeout_ms = 15000 })
                        local message = {id="pressure-final",role="assistant",content=mare.json.array({{type="text",text="pressure-done"}})}
                        ctx.bus:emit("item/completed", {turn_id=tid,item_id="pressure-final",status="completed",item={type="assistant_message",message=message}})
                        ctx.bus:emit("turn/completed", {turn_id=tid,status="completed",text="pressure-done",usage={},termination_reason="completed"})
                        finished_res = {turn_id=tid,status="completed",text="pressure-done",messages=mare.json.array({message}),usage={},termination_reason="completed",started_at_ms=0}
                        active_turn = nil
                        done_tx:send(finished_res)
                    end)
                    return { status = "started", turn_id = tid }
                end
                require("mare_lib").service(ctx, "agent", svc)
            end }
        "# .replace("__PREPARE__", &prepare).replace("__NOTICE_COUNT__", notice_count).replace("__FLOOD_BARRIER__", &json!(flood_url).to_string())).unwrap();
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        let mut command = mare_cmd();
        command.args(["-p", "--json", "--stream-deltas", "--model", "faux/echo", "--no-session", "--yolo"]);
        if timeout {
            command.args(["--timeout", "800ms"]);
        }
        let child = ChildGroup(
            command
                .arg("go")
                .env("MARE_HOME", home.path())
                .env("MARE_FAUX_SCRIPT", &script)
                .current_dir(home.path())
                .stdin(Stdio::null())
                .stdout(writer.try_clone().unwrap())
                .stderr(std::fs::File::create(home.path().join("stderr.log")).unwrap())
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let mut output = BufReader::new(reader);
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["type"], "session");
        Self { child, output, writer, flood_listener: Some(listener), _home: home }
    }

    fn accept_flood_barrier(&mut self, timeout: Duration) -> std::net::TcpStream {
        let listener = self.flood_listener.take().expect("flood_listener already taken");
        let barrier = AcceptBarrier::spawn(listener);
        barrier.recv_timeout(timeout).expect("producer stopped polling behind stdout").unwrap().0
    }
    pub(super) fn blocked(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let mut available = 0;
            assert_eq!(unsafe { libc::ioctl(self.output.get_ref().as_raw_fd(), libc::FIONREAD, &mut available) }, 0);
            let mut ready = libc::pollfd { fd: self.writer.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
            assert!(unsafe { libc::poll(&mut ready, 1, 0) } >= 0);
            if available > 0 && ready.revents & libc::POLLOUT == 0 {
                return;
            }
            assert!(
                self.child.0.try_wait().unwrap().is_none(),
                "process exited before backpressure: {}",
                std::fs::read_to_string(self._home.path().join("stderr.log")).unwrap_or_default()
            );
            assert!(Instant::now() < deadline, "stdout never filled");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn timeout_still_expires_when_an_open_stdout_reader_does_not_read() {
    let mut pressure = Pressure::new(true);
    pressure.blocked();
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = pressure.child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "--timeout was frozen behind stdout backpressure");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(4));
    // The actual reader has remained open and unread the entire time; this was not EPIPE.
}

#[test]
fn resumed_stdout_preserves_required_records_and_exact_lagged_order() {
    use std::io::Write;
    let mut pressure = Pressure::new(false);
    pressure.blocked();
    let mut stream = pressure.accept_flood_barrier(Duration::from_secs(15));
    // Drain the log's buffered prefix and its `Dropped` marker before releasing the flood
    // barrier so subsequent required records enter without backpressure.
    let mut records = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = pressure.output.read_line(&mut line).unwrap();
        assert!(n > 0, "stdout closed before buffered notices and Dropped marker drained");
        let record: Value = serde_json::from_str(&line).unwrap();
        let is_dropped = record["type"] == "dropped";
        records.push(record);
        if is_dropped {
            break;
        }
    }
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    let _ = stream.flush();
    drop(stream);
    // There are no additional writers in the test. Closing this duplicate permits real EOF.
    drop(pressure.writer);
    for line in pressure.output.lines() {
        records.push(serde_json::from_str::<Value>(&line.unwrap()).unwrap());
    }
    let status = pressure.child.0.wait().unwrap();
    assert!(
        status.success(),
        "mare exited with {status}; last record: {:?}; stderr:\n{}",
        records.last(),
        std::fs::read_to_string(pressure._home.path().join("stderr.log")).unwrap_or_default()
    );
    let mut notices = 0;
    let mut skipped = 0;
    let mut last_notice = 0;
    let mut dropped = None;
    let mut message = None;
    let mut event_seqs = Vec::new();
    for (index, record) in records.iter().enumerate() {
        if let Some(seq) = record.get("seq").and_then(Value::as_u64) {
            assert!(record["session_epoch"].is_string(), "{record}");
            event_seqs.push(seq);
        } else {
            assert!(
                matches!(record["type"].as_str(), Some("session" | "dropped" | "lagged" | "turn.failed" | "settled")),
                "unexpected record without seq: {record}"
            );
        }
        match record["type"].as_str().unwrap() {
            "host/notice" => {
                notices += 1;
                if let Some(number) = record["data"]["text"].as_str().unwrap().strip_prefix("pressure:") {
                    let number: usize = number.parse().unwrap();
                    assert!(number > last_notice, "notice order changed");
                    last_notice = number;
                }
                assert!(dropped.is_none(), "best-effort prefix appeared after its Dropped marker");
            }
            "dropped" => {
                assert!(dropped.is_none(), "expected a single Dropped marker");
                skipped += record["data"]["count"].as_u64().unwrap() as usize;
                dropped = Some(index);
            }
            "item/completed" if record["data"]["item"]["type"] == "assistant_message" => {
                assert!(message.is_none());
                message = Some(index);
            }
            _ => {}
        }
    }
    assert!(
        event_seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "dropped lossy events must not create seq gaps: {event_seqs:?}"
    );
    assert_eq!(notices, 1 + last_notice, "notice prefix must be contiguous before Dropped");
    let dropped_notices = 6001usize.checked_sub(notices).expect("more notices than emitted");
    assert!(dropped_notices > 0, "the paused reader must produce visible best-effort notice loss");
    assert_eq!(
        skipped, dropped_notices,
        "dropped count ({skipped}) must exactly match dropped notices ({dropped_notices})"
    );
    assert!(dropped.unwrap() < message.unwrap(), "Dropped must precede the required message");
    assert_eq!(records.last().unwrap()["type"], "settled");
    assert_eq!(records.last().unwrap()["data"]["text"], "pressure-done");
    assert_eq!(records.iter().filter(|r| r["type"] == "turn/completed").count(), 1);
    assert_eq!(records.iter().filter(|r| r["type"] == "settled").count(), 1);
}

#[test]
fn flood_barrier_thread_is_reclaimed_on_drop() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let barrier = AcceptBarrier::spawn_with_done(listener, Some(Arc::clone(&finished)));
    drop(barrier);
    assert!(finished.load(Ordering::Acquire), "listener thread was not joined/reclaimed on drop");
}

#[cfg(unix)]
struct NofileGuard {
    saved: libc::rlimit,
}

#[cfg(unix)]
impl NofileGuard {
    fn lower_to(limit: libc::rlim_t) -> Self {
        let mut rlim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) }, 0);
        let saved = rlim;
        rlim.rlim_cur = limit.min(rlim.rlim_max);
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) }, 0);
        Self { saved }
    }
}

#[cfg(unix)]
impl Drop for NofileGuard {
    fn drop(&mut self) {
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.saved) };
    }
}

#[cfg(unix)]
fn run_fd_exhaustion_barrier_drop_probe() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let barrier = AcceptBarrier::spawn_with_done(listener, Some(Arc::clone(&finished)));

    let rlim_guard = NofileGuard::lower_to(64);
    let mut held_fds = Vec::new();
    while let Ok(file) = std::fs::File::open("/dev/null") {
        held_fds.push(file);
    }
    assert!(std::net::TcpStream::connect(addr).is_err(), "connect must fail under FD exhaustion");

    let (tx, rx) = std::sync::mpsc::channel();
    let dropper = std::thread::spawn(move || {
        drop(barrier);
        let _ = tx.send(());
    });
    let drop_res = rx.recv_timeout(Duration::from_secs(1));

    drop(held_fds);
    drop(rlim_guard);

    if drop_res.is_err() {
        let _ = std::net::TcpStream::connect(addr);
        let _ = dropper.join();
        panic!("AcceptBarrier::drop hung when wakeup TcpStream::connect failed");
    }
    dropper.join().unwrap();
    assert!(finished.load(Ordering::Acquire), "listener thread was not joined/reclaimed on drop");
}

#[cfg(unix)]
#[test]
fn flood_barrier_drop_does_not_hang_when_wakeup_connect_fails() {
    const CHILD_ENV: &str = "MARE_TEST_FD_EXHAUSTION_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        run_fd_exhaustion_barrier_drop_probe();
        return;
    }

    let mut rlim_before = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim_before) }, 0);

    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(exe)
        .args([
            "--exact",
            "suite::frontends::headless_pressure::flood_barrier_drop_does_not_hang_when_wakeup_connect_fails",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .run_output();
    assert!(
        output.status.success(),
        "fd exhaustion child failed ({}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let mut rlim_after = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim_after) }, 0);
    assert_eq!(rlim_after.rlim_cur, rlim_before.rlim_cur, "parent RLIMIT_NOFILE must stay untouched");
}

#[cfg(unix)]
#[test]
fn nofile_guard_restores_rlimit_on_panic() {
    const CHILD_ENV: &str = "MARE_TEST_NOFILE_GUARD_PANIC_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        let mut before = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut before) }, 0);
        let res = std::panic::catch_unwind(|| {
            let _guard = NofileGuard::lower_to(64);
            panic!("simulated failure under lowered RLIMIT_NOFILE");
        });
        assert!(res.is_err());
        let mut after = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut after) }, 0);
        assert_eq!(after.rlim_cur, before.rlim_cur, "NofileGuard must restore RLIMIT_NOFILE on unwind");
        return;
    }

    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(exe)
        .args(["--exact", "suite::frontends::headless_pressure::nofile_guard_restores_rlimit_on_panic", "--nocapture"])
        .env(CHILD_ENV, "1")
        .run_output();
    assert!(
        output.status.success(),
        "NofileGuard panic child failed ({}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
