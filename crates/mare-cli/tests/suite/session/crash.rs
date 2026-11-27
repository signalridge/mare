//! Crash and clean close: what resume makes of a killed run.

use super::*;

#[tokio::test]
async fn interrupted_turn_continues_without_a_new_prompt() {
    local(async {
        let env = TestEnv::new().await;
        env.write_session("2026-01-01T00-00-00_test.jsonl", &[msg("u1", None, "user", "do the thing")]);
        let h = env.host().faux(json!([])).resume().build().await;
        assert_eq!(h.state().await["message_count"], 1);
        let res = h.continue_turn().await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        assert_eq!(h.state().await["message_count"], 2, "continue adds the answer, no new user message");
        assert_eq!(h.continue_turn().await.unwrap()["error"]["message"], "nothing to continue");
        h.shutdown().await;
    })
    .await
}

// --- crash windows: a real `mare` process is killed (SIGKILL) at each
// persistence window, then resumed with `-c`; the verdicts must be in the session file itself.

/// Test-only user plugin: an external tool `effect` (declared idempotent — which must not
/// matter), a read-only `peek`, and park points that block on the test barrier at one window.
const FAULT_PLUGIN: &str = r#"
return {
  name = "fault",
  inject = { "tools", "policy" },
  apply = function(ctx)
    local window, dir, park_url = mare.env("FAULT_WINDOW"), mare.env("FAULT_DIR"), mare.env("FAULT_URL")
    local function park(tag)
      if tag ~= window then return end
      mare.http.request({ url = park_url .. "?window=" .. tag, timeout_ms = 30000 })
    end
    local function obj() return { type = "object", properties = {} } end
    ctx.tools:register("effect", { description = "side effect", parameters = obj(),
      side_effect = "external", idempotent = true,
      execute = function()
        park("admitted")
        local prev = mare.fs.read(dir .. "/effects") or ""
        mare.fs.write(dir .. "/effects", prev .. "x")
        park("effect")
        return "effect ok"
      end })
    ctx.tools:register("peek", { description = "read", parameters = obj(), side_effect = "read",
      execute = function() return "peeked" end })
    ctx.policy:voter("tool/before", function(call) if call.name == "effect" then park("prepared") end end)
    ctx:on("item/started", function(ev)
      local it = ev.item
      if it and it.type == "tool_call" and it.name == "effect" then park("started") end
    end)
    ctx:on("item/completed", function(ev)
      local it = ev.item
      local m = it and it.message
      if m and m.role == "tool_result" and m.tool_name == "effect" then park("result") end
    end)
  end,
}
"#;

const STDERR_DIAG_MAX_BYTES: usize = 64 * 1024;

fn lossy_bounded_utf8(bytes: &[u8], max_bytes: usize) -> String {
    let mut s = String::from_utf8_lossy(bytes).into_owned();
    if s.len() > max_bytes {
        let mut end = max_bytes;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// The bounded read loop shared by the Unix and Windows readers. The deadline is checked at
/// the top of every iteration: a continuously readable pipe never reaches the wait path, so
/// that check is the only thing that bounds it short of the byte cap. `now` and `wait_readable`
/// are injected so a test can prove it with a reader that never runs dry.
fn read_bounded_loop(
    reader: &mut impl std::io::Read,
    deadline: std::time::Instant,
    mut now: impl FnMut() -> std::time::Instant,
    mut wait_readable: impl FnMut(std::time::Duration) -> bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let remaining = STDERR_DIAG_MAX_BYTES.saturating_sub(out.len());
        if remaining == 0 || now() >= deadline {
            break;
        }
        let cap = buf.len().min(remaining);
        match reader.read(&mut buf[..cap]) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let Some(rem) = deadline.checked_duration_since(now()) else { break };
                if !wait_readable(rem) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    out
}

#[cfg(unix)]
fn read_stderr_bounded(stderr: Option<std::process::ChildStderr>, timeout: std::time::Duration) -> String {
    use std::os::fd::AsRawFd;
    let Some(mut stderr) = stderr else {
        return String::new();
    };
    let fd = stderr.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
    let deadline = std::time::Instant::now() + timeout;
    let out = read_bounded_loop(&mut stderr, deadline, std::time::Instant::now, |rem| {
        let ms = rem.as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut pfd, 1, ms) > 0 }
    });
    lossy_bounded_utf8(&out, STDERR_DIAG_MAX_BYTES)
}

/// An anonymous pipe has no non-blocking read on Windows; peek first and report an empty
/// pipe as `WouldBlock` so the shared loop handles it like the Unix reader.
#[cfg(windows)]
struct PeekedPipe(std::process::ChildStderr);

#[cfg(windows)]
impl std::io::Read for PeekedPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, GetLastError, HANDLE};
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;
        let handle = self.0.as_raw_handle() as HANDLE;
        let mut avail = 0u32;
        let ok = unsafe {
            PeekNamedPipe(handle, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut())
        };
        if ok == 0 {
            if unsafe { GetLastError() } == ERROR_BROKEN_PIPE {
                return Ok(0);
            }
            return self.0.read(buf);
        }
        if avail == 0 {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        let n = (avail as usize).min(buf.len());
        self.0.read(&mut buf[..n])
    }
}

#[cfg(windows)]
fn read_stderr_bounded(stderr: Option<std::process::ChildStderr>, timeout: std::time::Duration) -> String {
    let Some(stderr) = stderr else {
        return String::new();
    };
    let deadline = std::time::Instant::now() + timeout;
    let out = read_bounded_loop(&mut PeekedPipe(stderr), deadline, std::time::Instant::now, |rem| {
        std::thread::park_timeout(rem.min(std::time::Duration::from_millis(5)));
        true
    });
    lossy_bounded_utf8(&out, STDERR_DIAG_MAX_BYTES)
}

#[cfg(not(any(unix, windows)))]
fn read_stderr_bounded(stderr: Option<std::process::ChildStderr>, timeout: std::time::Duration) -> String {
    use std::io::Read;
    let Some(mut stderr) = stderr else {
        return String::new();
    };
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let handle = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf) {
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
    while out.len() < STDERR_DIAG_MAX_BYTES {
        let Some(rem) = deadline.checked_duration_since(std::time::Instant::now()) else {
            break;
        };
        match rx.recv_timeout(rem) {
            Ok(chunk) => {
                let take = chunk.len().min(STDERR_DIAG_MAX_BYTES - out.len());
                out.extend_from_slice(&chunk[..take]);
            }
            Err(_) => break,
        }
    }
    drop(rx);
    let _ = handle.join();
    lossy_bounded_utf8(&out, STDERR_DIAG_MAX_BYTES)
}

fn await_park(
    child: &mut std::process::Child,
    barrier: &AcceptBarrier,
    timeout: std::time::Duration,
    window: &str,
) -> Result<std::net::TcpStream, String> {
    match barrier.recv_timeout(timeout) {
        Ok(Ok((stream, _))) => Ok(stream),
        Ok(Err(error)) => Err(format!("barrier listener failed at {window}: {error}")),
        Err(_) => {
            let exited = child.try_wait().unwrap();
            let pid = child.id();
            mare_binding::platform::signal_tree(Some(pid), mare_binding::platform::Signal::Kill);
            if exited.is_none() {
                let _ = child.wait();
            }
            let err = read_stderr_bounded(child.stderr.take(), std::time::Duration::from_millis(250));
            // Reparented descendants killed by SIGKILL may briefly remain visible to `kill(-pgid, 0)`
            // on macOS while PID 1 reaps them after stderr closes; wait boundedly for the group to clear.
            let reap_deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while mare_binding::platform::tree_alive(Some(pid)) && std::time::Instant::now() < reap_deadline {
                std::thread::sleep(std::time::Duration::from_millis(10)); // sleep-ok: bounded poll for OS reap of SIGKILLed process group
            }
            if let Some(st) = exited {
                return Err(format!("mare exited ({st}) before parking at {window}; stderr:\n{err}"));
            }
            Err(format!("never parked at {window}; stderr:\n{err}"))
        }
    }
}

/// Run `mare` until it parks at `window`, kill -9 it, resume with `-c`, and return the tool
/// results persisted in the session file (call id -> texts) plus how often `effect` ran.
fn crash_and_resume(window: &str) -> (std::collections::HashMap<String, Vec<String>>, String) {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(work.path()).unwrap();
    let dir = cwd.join(".fault");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(home.path().join("config/plugins")).unwrap();
    std::fs::write(home.path().join("config/plugins/fault.lua"), FAULT_PLUGIN).unwrap();
    let script = |name: &str, v: Value| {
        let p = dir.join(name);
        std::fs::write(&p, v.to_string()).unwrap();
        p
    };
    let first = script(
        "s1.json",
        json!([{ "tool_calls": [{ "name": "effect", "arguments": {} }, { "name": "peek", "arguments": {} }] }, { "text": "done" }]),
    );
    let second = script("s2.json", json!([{ "text": "ok" }]));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let park_url = format!("http://127.0.0.1:{}/park", listener.local_addr().unwrap().port());
    let run = |args: &[&str], faux: &std::path::Path, window: Option<&str>| {
        let mut c = mare_cmd();
        c.args(["--yolo", "--model", "faux/echo"])
            .args(args)
            .env("MARE_HOME", home.path())
            .env("MARE_FAUX_SCRIPT", faux)
            .env("FAULT_DIR", &dir)
            .env_remove("FAULT_WINDOW")
            .env_remove("FAULT_URL")
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(w) = window {
            c.env("FAULT_WINDOW", w);
            c.env("FAULT_URL", &park_url);
        }
        c
    };
    let barrier = AcceptBarrier::spawn(listener);
    let mut child = ChildGroup::spawn(&mut run(&["-p", "go"], &first, Some(window))).unwrap();
    // a hang ceiling only (a loaded runner can take seconds to start the child): WAIT_SECS
    let _parked = await_park(&mut child.0, &barrier, std::time::Duration::from_secs(crate::common::WAIT_SECS), window)
        .unwrap_or_else(|msg| panic!("{msg}"));
    drop(barrier);
    mare_binding::platform::signal_tree(Some(child.0.id()), mare_binding::platform::Signal::Kill);
    child.0.wait().unwrap();
    let out = run(&["-c", "-p", "what happened?"], &second, None).run_output();
    assert!(out.status.success(), "resume failed: {}", String::from_utf8_lossy(&out.stderr));

    let mut results: std::collections::HashMap<String, Vec<String>> = Default::default();
    let mut files = vec![];
    let mut stack = vec![home.path().join("state/sessions")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "jsonl") && !p.ends_with("index.jsonl") {
                files.push(p);
            }
        }
    }
    for f in files {
        for line in std::fs::read_to_string(&f).unwrap().lines() {
            let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
            let m = &e["message"];
            if e["type"] == "message" && m["role"] == "tool_result" {
                let text =
                    m["content"].as_array().map(|a| a.iter().filter_map(|b| b["text"].as_str()).collect::<String>());
                results
                    .entry(m["tool_call_id"].as_str().unwrap_or("").to_string())
                    .or_default()
                    .push(text.unwrap_or_default());
            }
        }
    }
    let effects = std::fs::read_to_string(dir.join("effects")).unwrap_or_default();
    (results, effects)
}

fn only(results: &std::collections::HashMap<String, Vec<String>>, id: &str) -> String {
    let v = results.get(id).unwrap_or_else(|| panic!("no persisted result for {id}: {results:?}"));
    assert_eq!(v.len(), 1, "exactly one result per call in the session file: {results:?}");
    v[0].clone()
}

#[test]
fn killed_after_prepared_resumes_as_never_started() {
    let (r, effects) = crash_and_resume("prepared");
    assert!(only(&r, "call_1_1").starts_with("interrupted: this call never started"), "{r:?}");
    assert!(only(&r, "call_1_2").starts_with("interrupted:"), "{r:?}");
    assert_eq!(effects, "", "nothing ran, nothing replayed");
}

#[test]
fn killed_after_started_resumes_as_outcome_unknown_even_if_idempotent() {
    let (r, effects) = crash_and_resume("started");
    let got = only(&r, "call_1_1");
    assert!(got.starts_with("outcome_unknown:") && got.contains("processes it started may still be running"), "{r:?}");
    assert!(only(&r, "call_1_2").starts_with("interrupted:"), "read-only call: {r:?}");
    assert_eq!(effects, "", "never auto-replayed");
}

/// Past the host's durable-start gate (`mare.exec.run` admitted the call on its
/// fsynced `started`), before the effect: still outcome-unknown, never replayed.
#[test]
fn killed_after_the_host_admitted_the_call_resumes_as_outcome_unknown() {
    let (r, effects) = crash_and_resume("admitted");
    assert!(only(&r, "call_1_1").starts_with("outcome_unknown:"), "{r:?}");
    assert_eq!(effects, "", "never auto-replayed");
}

#[test]
fn killed_after_the_side_effect_resumes_as_outcome_unknown_not_replayed() {
    let (r, effects) = crash_and_resume("effect");
    assert!(only(&r, "call_1_1").starts_with("outcome_unknown:"), "{r:?}");
    assert_eq!(effects, "x", "the effect happened once and was not replayed");
}

/// a resumed crashed session writes the results of its unfinished calls at
/// once — before any prompt — so they are in the file, a second resume adds none, and a fork
/// refuses the incomplete turn until a completion boundary has been recorded.
#[tokio::test]
async fn resuming_a_crash_mid_tool_persists_the_missing_results_once() {
    local(async {
        let env = TestEnv::new().await;
        let call = |id: &str, name: &str| json!({ "type": "tool_call", "id": id, "name": name, "arguments": {} });
        // no `state` record: written by a run that never closed it (a crash)
        let path = env.write_session(
            "2026-01-01T00-00-00_cr.jsonl",
            &[
                msg("m1", None, "user", "go"),
                json!({ "type": "message", "entry_id": "m2", "parent_entry_id": "m1",
                        "message": { "role": "assistant", "content": [call("c1", "bash"), call("c2", "read")] } }),
                json!({ "type": "tool_state", "tool_call_id": "c1", "name": "bash", "state": "started",
                        "side_effect": "external" }),
            ],
        );
        let results = |p: &std::path::Path| -> Vec<(String, String)> {
            std::fs::read_to_string(p)
                .unwrap()
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter(|e| e["type"] == "message" && e["message"]["role"] == "tool_result")
                .map(|e| {
                    let m = &e["message"];
                    (m["tool_call_id"].as_str().unwrap().to_string(), m["content"][0]["text"].as_str().unwrap().into())
                })
                .collect()
        };
        let h = env.host().build().await;
        expose(&h);
        global(&h, "P", path.to_string_lossy().to_string());
        let interrupted: bool = eval(&h, "S:load({ path = P }) return A:interrupted()").await;
        assert!(interrupted, "the turn can be continued from the written results");
        h.shutdown().await;
        let r = results(&path);
        assert_eq!(r.len(), 2, "one result per call, written at resume: {r:?}");
        assert!(r[0].0 == "c1" && r[0].1.starts_with("outcome_unknown:"), "{r:?}");
        assert!(r[1].0 == "c2" && r[1].1.starts_with("interrupted:"), "{r:?}");

        // crash again (no clean close after the next open) and resume: nothing is added twice
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut f, b"{\"type\":\"state\",\"state\":\"running\"}\n").unwrap();
        drop(f);
        let h = env.host().build().await;
        expose(&h);
        global(&h, "P", path.to_string_lossy().to_string());
        let _: bool = eval(&h, "S:load({ path = P }) return true").await;
        assert_eq!(results(&path).len(), 2, "a second resume adds nothing: {:?}", results(&path));
        let (ok, err): (bool, String) =
            eval(&h, "local ok, err = pcall(S.fork, S, { session_id = S:info().session_id }) return ok, tostring(err)")
                .await;
        assert!(!ok && err.contains("no recorded completed turn"), "{err}");
        h.shutdown().await;
    })
    .await
}

#[test]
fn killed_after_the_result_keeps_the_result() {
    let (r, effects) = crash_and_resume("result");
    assert_eq!(only(&r, "call_1_1"), "effect ok", "a persisted result is the completion: {r:?}");
    assert!(only(&r, "call_1_2").starts_with("interrupted:"), "{r:?}");
    assert_eq!(effects, "x");
}

/// Open / clean-close markers: a resume tells a crash from a clean exit,
/// and a request left without a reply by a clean shutdown isn't counted as a crash.
#[tokio::test]
async fn a_clean_close_is_told_from_a_crash_on_resume() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "a" }])).build().await;
        expose(&h);
        h.prompt("hi", json!({})).await.unwrap();
        let path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;
        let lines =
            |p: &str| -> Vec<String> { std::fs::read_to_string(p).unwrap().lines().map(String::from).collect() };
        let mut file = lines(&path);
        assert!(file.iter().any(|l| l.contains(r#""state":"running""#)), "{file:?}");
        assert!(file.last().unwrap().contains(r#""state":"closed""#), "a clean close is the last record: {file:?}");
        // a request marker without a reply, then the clean close (shutdown mid-request)
        let marker = json!({ "type": "custom", "kind": "agent/request", "data": { "request_id": "r1", "attempt": 1 } });
        file.insert(file.len() - 1, marker.to_string());
        std::fs::write(&path, file.join("\n") + "\n").unwrap();

        let h = env.host().faux(json!([])).resume().build().await;
        expose(&h);
        let (crashed, attempt): (bool, i64) =
            eval(&h, "local p = A:pending_request() return S:info().crashed == true, p and p.attempt or -1").await;
        assert!(!crashed);
        assert_eq!(attempt, 1, "a clean shutdown doesn't count the attempt");
        h.shutdown().await;

        // the same, killed: no "closed" after the last "running"
        let mut file = lines(&path);
        assert!(file.pop().unwrap().contains(r#""state":"closed""#));
        std::fs::write(&path, file.join("\n") + "\n").unwrap();
        let h = env.host().faux(json!([])).resume().build().await;
        expose(&h);
        let (crashed, attempt): (bool, i64) =
            eval(&h, "local p = A:pending_request() return S:info().crashed == true, p and p.attempt or -1").await;
        assert!(crashed, "no clean close after the last open");
        assert_eq!(attempt, 2, "a crash counts the attempt");
        // the markers stay out of the tree (never copied into a fork)
        let n: i64 = eval(
            &h,
            r#"local n = 0 for _, e in ipairs(S:entries()) do if e.type == "state" then n = n + 1 end end return n"#,
        )
        .await;
        assert_eq!(n, 0);
        h.shutdown().await;
    })
    .await
}

/// A clean close in the middle of a model request ends the core crash-guard mark (`release`):
/// the resumed retry of the same request counts no crash. A clean resume also ends a mark an
/// older run left behind, keeping the count of earlier real crashes.
#[tokio::test]
async fn a_clean_close_mid_request_counts_no_crash() {
    local(async {
        let env = TestEnv::new().await;
        let guard = env.home.path().join("state/crash-guard.json");
        let inflight = |g: &std::path::Path| -> Vec<(String, bool, u64)> {
            let v: Value =
                std::fs::read_to_string(g).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
            v["entries"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter(|(k, _)| k.starts_with("req:"))
                        .map(|(k, e)| (k.clone(), e["inflight"] == true, e["crashes"].as_u64().unwrap_or(0)))
                        .collect()
                })
                .unwrap_or_default()
        };
        let h = env.host().faux(json!([{ "text": "x".repeat(40_000), "delay_ms": 10 }])).build().await;
        expose(&h);
        exec(&h, r#"mare.task.spawn(function() A:prompt("go") end)"#).await;
        wait_until(10, "the request in flight", || {
            let g = guard.clone();
            async move { inflight(&g).iter().any(|e| e.1) }
        })
        .await;
        h.shutdown().await;
        let entries = inflight(&guard);
        assert!(
            !entries.is_empty() && entries.iter().all(|e| !e.1 && e.2 == 0),
            "released on the clean close: {entries:?}"
        );

        // a mark an older run left behind (one real crash before), then a clean close: the
        // resume ends it and keeps the earlier count
        let (id, ..) = entries[0].clone();
        // the cut-off turn may still land its own `finish` after the shutdown: let it
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        mare_binding::crashguard::begin(&guard, &id).unwrap();
        mare_binding::crashguard::begin(&guard, &id).unwrap();
        let h = env.host().faux(json!([{ "text": "done" }])).resume().build().await;
        expose(&h);
        exec(
            &h,
            r#"local orig = mare.crash.begin
               mare.crash.begin = function(fp) local n = orig(fp) FP, CRASHES = fp, n return n end"#,
        )
        .await;
        let pending: String =
            eval(&h, "local p = A:pending_request() return p and ('req:' .. p.request_id) or ''").await;
        assert_eq!(pending, id, "the interrupted request is picked up");
        let res: String = eval(
            &h,
            "local s = A:continue(); local r = s.turn_id and A:wait(s.turn_id) or s; return tostring(r.error)",
        )
        .await;
        assert_eq!(res, "nil");
        let (fp, crashes): (String, i64) = eval(&h, "return FP, CRASHES").await;
        assert_eq!(fp, id);
        assert_eq!(crashes, 1, "the earlier crash stays counted, the clean close adds none");
        h.shutdown().await;
    })
    .await
}

fn wait_for_child_ready(child: &mut ChildGroup) {
    use std::io::BufRead;
    use std::time::Duration;

    let stdout = child.0.stdout.take().expect("child stdout");
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = tx.send(result);
    });
    let ready = rx.recv_timeout(Duration::from_secs(10)).expect("child did not report readiness").unwrap();
    reader.join().unwrap();
    assert_eq!(ready.trim_end(), "ready");
}

#[test]
fn await_park_kills_stuck_child_and_returns_within_deadline() {
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 60 & echo stuck-diag >&2; echo ready; sleep 60"]);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args([
            "/c",
            "start /b ping -n 30 127.0.0.1 >NUL & echo stuck-diag 1>&2 & echo ready & ping -n 30 127.0.0.1 >NUL",
        ]);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    wait_for_child_ready(&mut child);
    let pid = child.0.id();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let barrier = AcceptBarrier::spawn_with_done(listener, Some(Arc::clone(&finished)));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut child = child;
        let res = await_park(&mut child.0, &barrier, Duration::from_millis(50), "stuck");
        let exited = child.0.try_wait().unwrap().is_some();
        let tree_alive = mare_binding::platform::tree_alive(Some(pid));
        drop(barrier);
        let _ = done_tx.send((res.map(|_| ()), exited, tree_alive));
    });

    let result = done_rx.recv_timeout(Duration::from_secs(2));
    if result.is_err() {
        // Kill the stuck process tree so the worker thread can unblock and exit cleanly.
        mare_binding::platform::signal_tree(Some(pid), mare_binding::platform::Signal::Kill);
    }
    let (res, exited, tree_alive) = result.expect("await_park hung waiting for stuck child stderr EOF");
    worker.join().unwrap();
    let err = res.expect_err("stuck child must time out");
    assert!(err.contains("never parked at stuck"), "{err}");
    assert!(err.contains("stuck-diag"), "{err}");
    assert!(exited, "stuck child must be killed before returning");
    assert!(!tree_alive, "stuck descendant process tree must be killed before await_park returns");
    assert!(finished.load(Ordering::Acquire), "listener accept thread leaked after await_park timeout");
}

#[test]
fn read_stderr_bounded_stops_at_deadline_and_byte_cap_when_pipe_stays_readable() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.args(["-c", "printf 'diag\\n' >&2; printf 'ready\\n'; while :; do printf '0123456789abcdef0123456789abcdef\\n' >&2; done"]);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args([
            "/c",
            "echo diag 1>&2 & echo ready & for /L %i in (1,0,2) do @echo 0123456789abcdef0123456789abcdef 1>&2",
        ]);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    wait_for_child_ready(&mut child);
    let stderr = child.0.stderr.take();
    let (tx, rx) = std::sync::mpsc::channel();
    let start = Instant::now();
    let worker = std::thread::spawn(move || {
        let out = read_stderr_bounded(stderr, Duration::from_millis(80));
        let _ = tx.send((out, start.elapsed()));
    });

    let result = rx.recv_timeout(Duration::from_secs(1));
    drop(child);
    let (out, elapsed) = result.expect("read_stderr_bounded hung on continuously readable stderr");
    worker.join().unwrap();
    assert!(!out.is_empty(), "expected diagnostic stderr bytes");
    assert!(elapsed < Duration::from_millis(500), "read_stderr_bounded exceeded deadline: {elapsed:?}");
    assert!(out.len() <= STDERR_DIAG_MAX_BYTES, "read_stderr_bounded exceeded byte cap: {}", out.len());
}

#[test]
fn read_stderr_bounded_caps_invalid_utf8_output_at_char_boundary() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.args(["-c", "head -c 65536 /dev/zero | tr '\\0' '\\377' >&2; sleep 60"]);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/c", "for /L %i in (1,1,2048) do @echo 0123456789abcdef0123456789abcdef 1>&2"]);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let stderr = child.0.stderr.take();
    let (tx, rx) = std::sync::mpsc::channel();
    let start = Instant::now();
    let worker = std::thread::spawn(move || {
        let out = read_stderr_bounded(stderr, Duration::from_millis(250));
        let _ = tx.send((out, start.elapsed()));
    });

    let result = rx.recv_timeout(Duration::from_secs(2));
    drop(child);
    let (out, elapsed) = result.expect("read_stderr_bounded hung on invalid UTF-8 stderr");
    worker.join().unwrap();
    assert!(!out.is_empty(), "expected diagnostic stderr output");
    assert!(elapsed < Duration::from_secs(1), "read_stderr_bounded exceeded deadline: {elapsed:?}");
    assert!(
        out.len() <= STDERR_DIAG_MAX_BYTES,
        "read_stderr_bounded exceeded UTF-8 byte cap after lossy conversion: {} > {}",
        out.len(),
        STDERR_DIAG_MAX_BYTES
    );
}

#[test]
fn read_bounded_loop_stops_at_deadline_while_the_reader_stays_readable_below_the_byte_cap() {
    use std::time::{Duration, Instant};

    // Never runs dry and never blocks: one byte per read, so the wait path is never taken
    // and the byte cap is ~64k reads away. Each clock reading advances 1 ms.
    struct Endless(usize);
    impl std::io::Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0 += 1;
            buf[0] = b'x';
            Ok(1)
        }
    }
    let start = Instant::now();
    let mut ticks = 0u32;
    let clock = || {
        ticks += 1;
        start + Duration::from_millis(u64::from(ticks))
    };
    let mut reader = Endless(0);
    let out = read_bounded_loop(&mut reader, start + Duration::from_millis(80), clock, |_| {
        panic!("a continuously readable reader must never wait")
    });
    assert_eq!(reader.0, 79, "reads must stop when the clock crosses the deadline");
    assert_eq!(out.len(), 79);
    assert!(out.len() < STDERR_DIAG_MAX_BYTES, "the byte cap must not be what stopped it");
}

#[test]
fn read_stderr_bounded_stops_at_deadline_before_byte_cap() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.args(["-c", "printf 'diag\\n' >&2; printf 'ready\\n'; sleep 60"]);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/c", "echo diag 1>&2 & echo ready & ping -n 30 127.0.0.1 >NUL"]);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(ready.trim_end(), "ready");
    drop(stdout);

    let stderr = child.0.stderr.take();
    let (tx, rx) = std::sync::mpsc::channel();
    let start = Instant::now();
    let worker = std::thread::spawn(move || {
        let out = read_stderr_bounded(stderr, Duration::from_millis(80));
        let _ = tx.send((out, start.elapsed()));
    });

    let result = rx.recv_timeout(Duration::from_secs(1));
    drop(child);
    let (out, elapsed) = result.expect("read_stderr_bounded hung without hitting byte cap");
    worker.join().unwrap();
    assert!(out.contains("diag"), "expected diagnostic line before deadline: {out:?}");
    assert!(out.len() < STDERR_DIAG_MAX_BYTES, "unexpectedly hit byte cap: {}", out.len());
    assert!(elapsed >= Duration::from_millis(50), "returned before deadline: {elapsed:?}");
    assert!(elapsed < Duration::from_millis(500), "read_stderr_bounded exceeded deadline: {elapsed:?}");
}

#[test]
fn child_group_drop_reclaims_descendant_tree_and_confirm_tree_exited_bounds_wait() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    let mut polls = 0usize;
    assert!(confirm_tree_exited(
        || {
            polls += 1;
            polls < 3
        },
        Duration::from_millis(200),
    ));
    assert!(!confirm_tree_exited(|| true, Duration::from_millis(20)));

    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 60 >/dev/null 2>&1 & printf 'ready\\n'; wait"]);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/c", "start /b ping -n 30 127.0.0.1 >NUL & echo ready & ping -n 30 127.0.0.1 >NUL"]);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(ready.trim_end(), "ready");
    drop(stdout);

    let pid = child.0.id();
    drop(child);
    assert!(!mare_binding::platform::tree_alive(Some(pid)), "ChildGroup::drop left descendant tree alive");
}

#[cfg(target_os = "linux")]
#[test]
fn child_group_drop_ignores_unreaped_zombie_descendants() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    const CHILD_ENV: &str = "MARE_TEST_SUBREAPER_ZOMBIE_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) }, 0);
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 60 >/dev/null 2>&1 & printf 'ready\\n'; wait"]);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = ChildGroup::spawn(&mut cmd).unwrap();
        let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
        let mut ready = String::new();
        stdout.read_line(&mut ready).unwrap();
        assert_eq!(ready.trim_end(), "ready");
        drop(stdout);

        let pid = child.0.id();
        drop(child);
        assert!(
            !mare_binding::platform::tree_alive(Some(pid)),
            "tree_alive treated unreaped zombie descendants as alive"
        );
        unsafe { while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {} }
        return;
    }

    let exe = std::env::current_exe().expect("current_exe");
    let output = Command::new(exe)
        .args(["--exact", "suite::session::crash::child_group_drop_ignores_unreaped_zombie_descendants", "--nocapture"])
        .env(CHILD_ENV, "1")
        .run_output();
    assert!(
        output.status.success(),
        "subreaper zombie probe failed ({}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[tokio::test]
async fn settle_persists_orphans_from_earlier_assistant_messages_too() {
    local(async {
        let env = TestEnv::new().await;
        let call = |id: &str, name: &str| json!({ "type": "tool_call", "id": id, "name": name, "arguments": {} });
        let path = env.write_session(
            "2026-01-01T00-00-00_multi_orphan.jsonl",
            &[
                msg("m1", None, "user", "first"),
                json!({ "type": "message", "entry_id": "m2", "parent_entry_id": "m1",
                        "message": { "role": "assistant", "content": [call("c_old", "bash")] } }),
                msg("m3", Some("m2"), "user", "second"),
                json!({ "type": "message", "entry_id": "m4", "parent_entry_id": "m3",
                        "message": { "role": "assistant", "content": [call("c_last", "read")] } }),
            ],
        );
        let h = env.host().build().await;
        expose(&h);
        global(&h, "P", path.to_string_lossy().to_string());
        let _: bool = eval(&h, "S:load({ path = P }) return true").await;
        h.shutdown().await;

        let persisted: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|e| e["type"] == "message" && e["message"]["role"] == "tool_result")
            .filter_map(|e| e["message"]["tool_call_id"].as_str().map(str::to_string))
            .collect();
        assert!(persisted.contains(&"c_old".to_string()), "earlier orphan c_old was not persisted: {persisted:?}");
        assert!(persisted.contains(&"c_last".to_string()), "last orphan c_last was not persisted: {persisted:?}");
        assert_eq!(persisted.len(), 2, "each orphan call must be persisted exactly once: {persisted:?}");
    })
    .await
}

#[tokio::test]
async fn settle_earlier_orphan_then_compact_local_remote_and_summarize_only() {
    local(async {
        let env = TestEnv::new().await;
        let call = |id: &str, name: &str| json!({ "type": "tool_call", "id": id, "name": name, "arguments": {} });
        let pad_a = "alpha ".repeat(400);
        let pad_b = "beta ".repeat(400);
        let path = env.write_session(
            "2026-01-01T00-00-00_orphan_compact.jsonl",
            &[
                msg("m1", None, "user", &format!("first request {pad_a}")),
                json!({ "type": "message", "entry_id": "m2", "parent_entry_id": "m1",
                        "message": { "role": "assistant", "content": [call("c_old", "bash")] } }),
                msg("m3", Some("m2"), "user", &format!("second request {pad_b}")),
                json!({ "type": "message", "entry_id": "m4", "parent_entry_id": "m3",
                        "message": { "role": "assistant", "content": [call("c_last", "read")] } }),
            ],
        );
        let h = env
            .host()
            .builtins(&["compact", "tools"])
            .config(json!({
                "plugins": {
                    "compact": {
                        "keep_recent_tokens": 120,
                        "recent_user_tokens": 40,
                        "remote": true
                    }
                }
            }))
            .build()
            .await;
        expose(&h);
        global(&h, "P", path.to_string_lossy().to_string());
        let (sum_ok, sum_err, local_ok, local_err): (bool, String, bool, String) = eval(
            &h,
            r#"
                local L = assert(C.cordis:get("llm"))
                local K = assert(C.cordis:get("compact"))

                -- Strict provider adjacency validator: every assistant tool_call must be immediately
                -- followed by its matching tool_result(s), with no interleaved user/assistant message
                -- and no orphan tool_result.
                local function validate_adjacency(msgs)
                    local i = 1
                    local saw_calls = 0
                    while i <= #msgs do
                        local m = msgs[i]
                        if m.role == "tool_result" then
                            return false, "orphan or out-of-order tool_result at " .. i .. ": " .. tostring(m.tool_call_id)
                        end
                        if m.role == "assistant" then
                            local need = {}
                            local count = 0
                            for _, b in ipairs(m.content or {}) do
                                if b.type == "tool_call" and b.id then
                                    need[b.id] = true
                                    count = count + 1
                                    saw_calls = saw_calls + 1
                                end
                            end
                            local j = i + 1
                            local got = 0
                            while j <= #msgs and msgs[j].role == "tool_result" do
                                local cid = msgs[j].tool_call_id
                                if not need[cid] then
                                    return false, "mismatched tool_result at " .. j .. ": " .. tostring(cid)
                                end
                                need[cid] = nil
                                got = got + 1
                                j = j + 1
                            end
                            if got ~= count then
                                return false, "missing adjacent tool_result after assistant at " .. i
                            end
                            i = j
                        else
                            i = i + 1
                        end
                    end
                    if saw_calls < 1 then
                        return false, "expected at least one tool_call in compacted request"
                    end
                    return true
                end

                local orig_complete = L.complete
                L.complete = function(self, req, ...)
                    local ok, verr = validate_adjacency(req.messages or {})
                    if not ok then
                        return {
                            role = "assistant",
                            content = mare.json.array(),
                            stop_reason = "error",
                            error = "complete rejected message order: " .. verr,
                            error_kind = "invalid",
                        }
                    end
                    return orig_complete(self, req, ...)
                end

                L:register_provider({
                    provider = "strictprobe",
                    is_configured = function() return true end,
                    models = { { model_id = "m", name = "Strict Probe", context_window = 100000, max_output = 4096 } },
                    stream = function(req)
                        return {
                            role = "assistant",
                            stop_reason = "stop",
                            content = { { type = "text", text = "compact summary ok" } },
                            usage = { input = 10, output = 10, cache_read = 0, cache_write = 0 },
                        }
                    end,
                })
                L:set_model("strictprobe/m", { save = false })
                VALIDATE_ADJACENCY = validate_adjacency

                -- 1. Load crashed session -> orphans.settle() appends c_old and c_last results at tail.
                S:load({ path = P })

                -- (a) summarize_only (used by /tree & /rewind branch summaries)
                local raw_msgs = A:messages()
                local s_ok, s_res = pcall(K.run, K, "summarize branch", { summarize_only = true, messages = raw_msgs })
                local sum_ok = s_ok and type(s_res) == "table" and type(s_res.summary) == "string"
                local sum_err = sum_ok and "" or tostring(type(s_res) == "table" and s_res.message or s_res)

                -- (b) local compaction (focus forces local stage; small keep_recent_tokens splits between m2 and m5)
                local l_ok, l_res = pcall(K.run, K, "keep facts", { reason = "manual" })
                local local_ok = l_ok and type(l_res) == "table" and l_res.status == "compacted"
                local local_err = local_ok and "" or tostring(type(l_res) == "table" and l_res.message or l_res)

                return sum_ok, sum_err, local_ok, local_err
                "#,
            )
            .await;

        // Test remote compaction on a fresh copy of the crashed session with settled tail orphans!
        let path_remote = env.write_session(
            "2026-01-01T00-00-01_orphan_remote.jsonl",
            &[
                msg("m1", None, "user", &format!("first request {pad_a}")),
                json!({ "type": "message", "entry_id": "m2", "parent_entry_id": "m1",
                        "message": { "role": "assistant", "content": [call("c_old", "bash")] } }),
                msg("m3", Some("m2"), "user", &format!("second request {pad_b}")),
                json!({ "type": "message", "entry_id": "m4", "parent_entry_id": "m3",
                        "message": { "role": "assistant", "content": [call("c_last", "read")] } }),
            ],
        );
        global(&h, "P_REMOTE", path_remote.to_string_lossy().to_string());
        let (r_ok, r_called, r_err): (bool, bool, String) = eval(
            &h,
            r#"
            local L = assert(C.cordis:get("llm"))
            local K = assert(C.cordis:get("compact"))
            local called = false
            L.supports_remote_compact = function() return true end
            L.compact_remote = function(_, req)
                called = true
                local ok, verr = VALIDATE_ADJACENCY(req.messages or {})
                if not ok then
                    return nil, "remote compact rejected message order: " .. verr, "invalid_request"
                end
                return {
                    provider = "strictprobe",
                    model = "m",
                    items = { { type = "compaction", encrypted_content = "enc" } },
                }
            end
            S:load({ path = P_REMOTE })
            local ok, res = pcall(K.run, K, nil, { reason = "manual" })
            local rok = ok and type(res) == "table" and res.status == "compacted"
            return rok, called, rok and "" or tostring(type(res) == "table" and res.message or res)
            "#,
        )
        .await;

        h.shutdown().await;
        assert!(sum_ok, "summarize_only failed on settled earlier orphan: {sum_err}");
        assert!(local_ok, "local compact failed on settled earlier orphan: {local_err}");
        assert!(r_called, "remote compact stage must have been invoked");
        assert!(r_ok, "remote compact failed on settled earlier orphan: {r_err}");
    })
    .await
}
