use super::assert_trees_die;
use crate::common::*;

#[tokio::test]
async fn wait_honours_the_deadline_after_streams_close() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).no_session().build().await;
        let (timed_out, ms): (bool, f64) = eval(
            &h,
            r#"local t0 = mare.task.clock()
               local r = mare.proc.run({ cmd = "sh", args = { "-c", "exec >&- 2>&-; sleep 30" }, timeout_ms = 300 })
               return r.timed_out, mare.task.clock() - t0"#,
        )
        .await;
        assert!(timed_out && ms < 5000.0, "{ms}ms");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn spill_file_holds_the_complete_output() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).no_session().build().await;
        let spill = env.cwd.join("spill.log");
        global(&h, "SPILL", spill.to_string_lossy().to_string());
        let (dropped, in_mem): (i64, usize) = eval(
            &h,
            r#"local r = mare.proc.run({ cmd = "sh", args = { "-c", "seq 1 50000" }, max_output = 1000, spill = SPILL })
               return r.dropped, #r.stdout"#,
        )
        .await;
        assert!(dropped > 0 && in_mem <= 1000);
        assert_eq!(std::fs::read_to_string(&spill).unwrap().lines().count(), 50000);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn timeout_and_cancel_end_the_whole_process_tree() {
    local(assert_trees_die(
        r#"local a = mare.proc.run({ cmd = "sh", args = { "-c", TREE("timeout") }, timeout_ms = 3000 })
           local tok = mare.task.token()
           mare.task.spawn(function() PID_UP("cancel"); tok:cancel() end)
           local b = mare.proc.run({ cmd = "sh", args = { "-c", TREE("cancel") }, cancel = tok })
           assert(a.timed_out and b.cancelled)"#,
        &["timeout", "cancel"],
    ))
    .await
}

#[tokio::test]
async fn trees_ignoring_sigterm_are_killed_after_the_grace_and_the_stop_is_confirmed() {
    // SIGTERM → grace → SIGKILL the group → bounded reap; confirmed stops
    // report no stop_unconfirmed, and job:stop / proc.run never block past the grace + reap
    local(assert_trees_die(
        r#"local stubborn = function(name) return "trap '' TERM; " .. TREE(name) end
           -- the timeout only has to outlast the shell's start (the trap must be installed before
           -- the kill); the bound asserted is what the stop adds on top of it
           local timeout_ms = 3000
           local t0 = mare.task.clock()
           local r = mare.proc.run({ cmd = "sh", args = { "-c", stubborn("run") }, timeout_ms = timeout_ms, kill_grace_ms = 300 })
           assert(r.timed_out and not r.stop_unconfirmed, "run: " .. tostring(r.stop_unconfirmed))
           assert(mare.task.clock() - t0 < timeout_ms + 5000, "run stop is bounded")
           local j = mare.proc.start({ cmd = "sh", args = { "-c", stubborn("job") } })
           PID_UP("job")
           local t1 = mare.task.clock()
           local s = j:stop({ grace_ms = 300 })
           assert(s.stopped and not s.stop_unconfirmed and j:status().state == "exited", "job stop")
           assert(mare.task.clock() - t1 < 5000, "stops are bounded")"#,
        &["run", "job"],
    ))
    .await
}

#[tokio::test]
async fn job_kill_and_dropped_handle_end_the_whole_process_tree() {
    local(assert_trees_die(
        r#"J = mare.proc.start({ cmd = "sh", args = { "-c", TREE("kill") } })
           local d = mare.proc.start({ cmd = "sh", args = { "-c", TREE("drop") } })
           PID_UP("kill"); PID_UP("drop")
           J:kill("KILL")
           d = nil
           collectgarbage("collect"); collectgarbage("collect")"#,
        &["kill", "drop"],
    ))
    .await
}

#[tokio::test]
async fn job_ring_buffer_keeps_head_and_tail_and_reports_dropped_bytes() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).no_session().build().await;
        let (dropped, text, peek, done): (i64, String, String, bool) = eval(
            &h,
            r#"local j = mare.proc.start({ cmd = "sh", args = { "-c", "printf 'FIRST'; seq 1 2000; printf 'LAST'" }, buffer_bytes = 100 })
               local st = j:wait({ timeout_ms = 5000 })
               assert(st.state == "exited" and st.code == 0)
               local r = j:read({ max_bytes = 1000 })
               local p = j:read_at(0, { max_bytes = 5 })
               return r.dropped, r.text, p.text, j:read().done"#,
        )
        .await;
        // head + tail survive (first errors aren't lost), the dropped middle is counted and marked
        assert!(dropped > 0, "dropped={dropped}");
        assert!(text.starts_with("FIRST") && text.ends_with("LAST"), "{text}");
        assert!(text.contains("bytes omitted"), "{text}");
        assert_eq!(peek, "FIRST", "read_at(0) peeks the kept head");
        assert!(done);
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn spawned_commands_lead_their_own_session() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).no_session().build().await;
        // setsid (Codex detach_from_tty): the shell is a session leader, so a terminal hangup or
        // signal to mare's group doesn't reach it, and stop_tree's group signals cover its tree
        let out: String = eval(
            &h,
            r#"local r = mare.proc.run({ cmd = "sh", args = { "-c", 'test "$(ps -o pgid= -p $$ | tr -d " ")" = "$$" && echo leader' } })
               return r.stdout"#,
        )
        .await;
        assert_eq!(out.trim(), "leader");
        h.shutdown().await;
    })
    .await
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn spawned_commands_get_the_parent_death_signal() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).no_session().build().await;
        // PR_SET_PDEATHSIG (Codex process_group.rs): the child gets SIGTERM if mare dies
        let out: String = eval(
            &h,
            r#"local r = mare.proc.run({ cmd = "python3", args = { "-c",
                 "import ctypes;l=ctypes.CDLL(None);v=ctypes.c_int();l.prctl(2,ctypes.byref(v));print(v.value)" } })
               return r.stdout"#,
        )
        .await;
        assert_eq!(out.trim(), "15", "SIGTERM");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn command_output_is_decoded_not_mangled() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).no_session().build().await;
        // non-UTF-8 output is decoded (Codex guess: Windows-1252 smart quotes, not Cyrillic),
        // UTF-8 stays whole (pi-rust output_decode)
        let out: String = eval(
            &h,
            r#"local r = mare.proc.run({ cmd = "sh", args = { "-c", "printf '\\223test\\224\\n'; printf '\\344\\270\\255\\346\\226\\207\\n'" } })
               return r.stdout"#,
        )
        .await;
        assert_eq!(out, "\u{201c}test\u{201d}\n中文\n");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn a_shell_that_exits_with_background_children_returns_promptly() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).no_session().build().await;
        // the background sleep keeps the pipes open; run returns once the shell exits (pi bash.ts)
        let (out, bg, ms): (String, bool, f64) = eval(
            &h,
            r#"local t0 = mare.task.clock()
               local r = mare.proc.run({ cmd = "sh", args = { "-c", "sleep 30 & echo hi" } })
               return r.stdout, r.background_left == true, mare.task.clock() - t0"#,
        )
        .await;
        assert_eq!(out.trim(), "hi");
        assert!(bg && ms < 5000.0, "background_left={bg} after {ms}ms");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn an_interrupt_is_sigint_before_the_hard_stop() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).no_session().build().await;
        // the command's INT handler runs (Codex: interrupt first, then terminate the group)
        let out: String = eval(
            &h,
            r#"local tok = mare.task.token()
               mare.task.spawn(function() mare.task.sleep(300); tok:cancel() end)
               local r = mare.proc.run({ cmd = "sh", args = { "-c", "trap 'echo got-int; exit 0' INT; sleep 30 & wait" }, cancel = tok })
               return r.stdout"#,
        )
        .await;
        assert!(out.contains("got-int"), "{out}");
        h.shutdown().await;
    })
    .await
}

// A headless tool creates a real process group with a signal-aware process and a
// stubborn descendant. The socket is the readiness/interrupt barrier; the pid
// file lets the guard clean up even if an assertion fails before that barrier.
// Adapted from reference implementation
// (armed-signal readiness and cleanup) and Codex unified_exec.rs:2556-2573.
#[cfg(unix)]
struct HeadlessTreeGuard {
    child: tokio::process::Child,
    group_file: std::path::PathBuf,
    group: Option<i32>,
    armed: bool,
}

#[cfg(unix)]
impl Drop for HeadlessTreeGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let group = self.group.or_else(|| std::fs::read_to_string(&self.group_file).ok()?.trim().parse().ok());
        if let Some(group) = group.filter(|group| *group > 1 && *group != unsafe { libc::getpgrp() }) {
            // SAFETY: the fixture records its own process group, never the test runner's.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        let _ = self.child.start_kill();
    }
}

#[cfg(unix)]
async fn headless_interrupt_tree(force: bool) {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::time::timeout;

    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("tree.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let group_file = home.path().join("tree.pgid");
    std::fs::write(
        home.path().join("tree.py"),
        r#"import os, signal, socket
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect('tree.sock')
with open('tree.pgid', 'w') as f:
    f.write(str(os.getpgrp()))
r, w = os.pipe()
child = os.fork()
if child == 0:
    os.close(r)
    s.close()
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    os.write(w, b'R')
    os.close(w)
    while True:
        signal.pause()
os.close(w)
signal.signal(signal.SIGINT, lambda *_: s.sendall(b'interrupt\n'))
signal.signal(signal.SIGTERM, lambda *_: s.sendall(b'terminate\n'))
assert os.read(r, 1) == b'R'
os.close(r)
s.sendall(('ready %d %d %d\n' % (os.getpid(), child, os.getpgrp())).encode())
while True:
    signal.pause()
"#,
    )
    .unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "command": "exec python3 tree.py" } }] },
            { "text": "done" }
        ])
        .to_string(),
    )
    .unwrap();
    let mut command = tokio::process::Command::from(mare_cmd());
    command
        .args(["-p", "--json", "--model", "faux/echo", "--no-session", "--clean", "--yolo", "run it"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if force {
        // The direct process path has a documented INT grace, so the second
        // signal is sent while the first cancellation is still in progress.
        command.args(["--set", "plugins.jobs.enabled=false", "--set", "plugins.tools.shell_session=fresh"]);
    }
    let child = command.spawn().unwrap();
    let mut guard = HeadlessTreeGuard { child, group_file, group: None, armed: true };
    let mut stdout = guard.child.stdout.take().unwrap();
    let output = tokio::spawn(async move {
        let mut text = String::new();
        stdout.read_to_string(&mut text).await.unwrap();
        text
    });
    let mut stderr = BufReader::new(guard.child.stderr.take().unwrap());
    let (stream, _) = timeout(Duration::from_secs(60), listener.accept()).await.expect("tool did not connect").unwrap();
    let mut stream = BufReader::new(stream);
    let mut ready = String::new();
    timeout(Duration::from_secs(60), stream.read_line(&mut ready)).await.expect("tool did not become ready").unwrap();
    let parts: Vec<_> = ready.split_whitespace().collect();
    assert_eq!(parts.len(), 4, "invalid readiness record: {ready:?}");
    assert_eq!(parts[0], "ready");
    let root: u32 = parts[1].parse().unwrap();
    let descendant: u32 = parts[2].parse().unwrap();
    let group: i32 = parts[3].parse().unwrap();
    assert!(group > 1 && group != unsafe { libc::getpgrp() }, "fixture must have its own process group");
    guard.group = Some(group);
    assert!(pid_alive(root) && pid_alive(descendant), "both tool processes must be running before SIGINT");
    assert!(mare_binding::platform::tree_alive(Some(group as u32)), "the tool process group must be live");

    let mare_pid = guard.child.id().unwrap() as i32;
    // SAFETY: this is the isolated mare child created above.
    assert_eq!(unsafe { libc::kill(mare_pid, libc::SIGINT) }, 0);
    let mut diagnostics = String::new();
    timeout(Duration::from_secs(30), async {
        loop {
            let mut line = String::new();
            assert_ne!(stderr.read_line(&mut line).await.unwrap(), 0, "headless exited before acknowledging SIGINT");
            diagnostics.push_str(&line);
            if line.contains("cancelling the turn") {
                break;
            }
        }
    })
    .await
    .expect("headless did not acknowledge the first SIGINT");

    if force {
        let mut interrupted = String::new();
        timeout(Duration::from_secs(30), stream.read_line(&mut interrupted))
            .await
            .expect("tool did not receive its interrupt")
            .unwrap();
        assert_eq!(interrupted, "interrupt\n", "first cancellation must reach the tool");
        assert!(guard.child.try_wait().unwrap().is_none(), "first SIGINT must leave time for cleanup");
        assert!(pid_alive(root) && pid_alive(descendant), "second SIGINT must exercise a live process tree");
        // SAFETY: the same isolated mare child is still running.
        assert_eq!(unsafe { libc::kill(mare_pid, libc::SIGINT) }, 0);
    }

    let deadline = if force { Duration::from_secs(3) } else { Duration::from_secs(15) };
    let status = timeout(deadline, guard.child.wait()).await.expect("headless did not exit after SIGINT").unwrap();
    assert_eq!(status.code(), Some(130), "headless interrupt exit: {status}; {diagnostics}");
    let output = timeout(Duration::from_secs(10), output).await.expect("headless stdout remained open").unwrap();
    if force {
        let mut tail = String::new();
        timeout(Duration::from_secs(10), stream.read_to_string(&mut tail))
            .await
            .expect("tool socket remained open")
            .unwrap();
        assert!(tail.is_empty(), "graceful termination reached the tool before forced exit: {tail:?}");
    } else {
        let settled: serde_json::Value =
            serde_json::from_str(output.lines().last().expect("no headless output")).unwrap();
        assert_eq!(settled["type"], "settled", "headless must finish the cancelled turn: {output}");
        assert_eq!(settled["data"]["reason"], "interrupted", "{settled}");
        assert_eq!(settled["data"]["exit_code"], 130, "{settled}");
    }
    wait_until(WAIT_SECS, "headless tool and descendant exit", || async move {
        !pid_alive(root) && !pid_alive(descendant) && !mare_binding::platform::tree_alive(Some(group as u32))
    })
    .await;
    guard.armed = false;
}

#[cfg(unix)]
#[tokio::test]
async fn headless_first_sigint_cancels_the_tool_and_reaps_its_process_tree() {
    headless_interrupt_tree(false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn headless_second_sigint_forces_exit_and_reaps_the_live_process_tree() {
    headless_interrupt_tree(true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn stopping_a_pty_sends_the_interrupt_character_first() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).no_session().build().await;
        // a program that handles Ctrl+C itself gets to: \x03 arrives before any signal
        let out: String = eval(
            &h,
            r#"local p = mare.pty.spawn({ cmd = "sh", args = { "-c", "trap 'echo got-ctrl-c; exit 0' INT; while :; do sleep 0.1; done" } })
               mare.task.sleep(300)
               p:stop({ grace_ms = 2000 })
               local seen = ""
               while true do
                   local chunk = p:read()
                   if not chunk then break end
                   seen = seen .. chunk
               end
               return seen"#,
        )
        .await;
        assert!(out.contains("got-ctrl-c"), "{out:?}");
        h.shutdown().await;
    })
    .await
}

/// writing to a terminal whose program isn't reading must not stall the (single-threaded)
/// runtime: input goes through a writer thread (Codex utils/pty), timers keep running
#[cfg(unix)]
#[tokio::test]
async fn pty_write_does_not_block_the_runtime() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).no_session().build().await;
        let ms: f64 = eval(
            &h,
            r#"local p = mare.pty.spawn({ cmd = "sh", args = { "-c", "stty raw -echo; sleep 2" } })
               mare.task.sleep(300)
               local t0 = mare.task.clock()
               local chunk = string.rep("x", 16384)
               for _ = 1, 256 do
                   local ok, err = pcall(function() p:write(chunk) end)
                   if not ok then
                       assert(tostring(err):find("PTY_INPUT_UNAVAILABLE", 1, true), tostring(err))
                       break
                   end
               end
               local ms = mare.task.clock() - t0
               p:kill()
               return ms"#,
        )
        .await;
        assert!(ms < 1000.0, "write blocked for {ms}ms");
        h.shutdown().await;
    })
    .await
}

/// macOS has no parent-death signal: a running background group is also recorded on disk
/// (`<state>/pgroups/<mare pid>`) so the next mare can end it if this one is killed; the record
/// goes when the group does. The sweep itself: mare-binding `pgroups` tests.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn background_groups_are_registered_on_disk_until_they_end() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).no_session().build().await;
        let reg = env.home.path().join("state/pgroups").join(std::process::id().to_string());
        let pid: i64 = eval(&h, r#"J = mare.proc.start({ cmd = "sleep", args = { "30" } }); return J:pid()"#).await;
        let text = std::fs::read_to_string(&reg).unwrap_or_default();
        assert!(text.starts_with(&format!("owner {} ", std::process::id())), "{text:?}");
        assert!(text.lines().any(|l| l.starts_with(&format!("{pid} "))), "{text:?}");
        exec(&h, r#"J:stop({ grace_ms = 200 })"#).await;
        let r = reg.clone();
        wait_until(10, "the registry to drop the ended group", move || {
            let r = r.clone();
            async move { !r.exists() }
        })
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn proc_service_reaps_processes_and_lingering_descendants_on_reload_and_disconnect() {
    local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["a", "b"]);
        let h = env.host().builtins(&["proc"]).user_plugins().no_session().build().await;
        expose(&h);
        let dir = env.cwd.to_string_lossy().to_string();
        global(&h, "DIR", dir);

        let read_pid = |name: &str| {
            let p = env.cwd.join(format!("{name}.pid"));
            let label = format!("{name}.pid populated");
            async move {
                wait_until(10, &label, || {
                    let p = p.clone();
                    async move { std::fs::read_to_string(&p).is_ok_and(|s| !s.trim().is_empty()) }
                })
                .await;
                std::fs::read_to_string(&p).unwrap().trim().parse::<u32>().unwrap()
            }
        };

        // 1. Caller plugin reloads: both root process and lingering descendant spawned via ctx.proc are reaped.
        let pids_a: String = eval(
            &h,
            r#"
            local p = assert(mare._pctx["a"]:get("proc"), "proc service unavailable for plugin a")
            local cmd = "sleep 30 & echo $! > " .. DIR .. "/reload-caller-child.pid; echo $$ > " .. DIR .. "/reload-caller-root.pid; exec sleep 30"
            p:spawn({ process_id = "p-reload-caller", command = { "sh", "-c", cmd } })
            return "spawned"
            "#,
        )
        .await;
        assert_eq!(pids_a, "spawned");
        let root_a = read_pid("reload-caller-root").await;
        let child_a = read_pid("reload-caller-child").await;
        assert!(pid_alive(root_a) && pid_alive(child_a));

        // Non-owner cannot control another owner's process_id.
        let non_owner_err: String = eval(
            &h,
            r#"
            local pb = assert(mare._pctx["b"]:get("proc"))
            local ok, err = pcall(function() pb:kill({ process_id = "p-reload-caller" }) end)
            return ok and "" or tostring(err)
            "#,
        )
        .await;
        assert!(!non_owner_err.is_empty(), "non-owner kill must be refused");

        h.kernel.reload(Some("ctx-a")).await.unwrap();
        wait_until(10, "caller-reload root and descendant reaped", || async move {
            !pid_alive(root_a) && !pid_alive(child_a)
        })
        .await;

        // 2. Service plugin (`proc`) reloads: processes owned by surviving callers are also reaped.
        exec(
            &h,
            r#"
            local p = assert(mare._pctx["b"]:get("proc"))
            local cmd = "sleep 30 & echo $! > " .. DIR .. "/reload-svc-child.pid; echo $$ > " .. DIR .. "/reload-svc-root.pid; exec sleep 30"
            p:spawn({ process_id = "p-reload-svc", command = { "sh", "-c", cmd } })
            "#,
        )
        .await;
        let root_b = read_pid("reload-svc-root").await;
        let child_b = read_pid("reload-svc-child").await;
        assert!(pid_alive(root_b) && pid_alive(child_b));

        h.kernel.reload(Some("proc")).await.unwrap();
        wait_until(10, "proc-reload root and descendant reaped", || async move {
            !pid_alive(root_b) && !pid_alive(child_b)
        })
        .await;

        // 3. Connection context disconnect (`rpc:connect` -> spawn -> `rpc:disconnect`): reaped with 0 orphans.
        exec(
            &h,
            r#"
            local rpc = assert(C.cordis:get("rpc"))
            rpc:connect("test-conn-1")
            local conn_ctx = assert(rpc:context("test-conn-1"))
            local p = assert(conn_ctx:get("proc"))
            local cmd = "sleep 30 & echo $! > " .. DIR .. "/dc-child.pid; echo $$ > " .. DIR .. "/dc-root.pid; exec sleep 30"
            p:spawn({ process_id = "p-dc", command = { "sh", "-c", cmd } })
            "#,
        )
        .await;
        let root_c = read_pid("dc-root").await;
        let child_c = read_pid("dc-child").await;
        assert!(pid_alive(root_c) && pid_alive(child_c));

        exec(
            &h,
            r#"
            local r = C.cordis:get("rpc"):disconnect("test-conn-1")
            local ok, cordis = pcall(require, "cordis")
            if ok and cordis.future.isThenable(r) then cordis.await(r) end
            "#,
        )
        .await;
        wait_until(10, "disconnect root and descendant reaped", || async move {
            !pid_alive(root_c) && !pid_alive(child_c)
        })
        .await;

        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn shell_exiting_with_pipe_holding_descendant_is_tracked_as_background_job() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["tools", "jobs"])
            .config(serde_json::json!({ "plugins": { "tools": { "shell_session": "fresh" } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        let (t_out, job_id, job_status, direct_bg, direct_state): (String, String, String, bool, String) = eval(
            &h,
            r#"
            local T = assert(C.cordis:get("tools"))
            local J = assert(C.cordis:get("jobs"))
            local res = T:execute({ id = "c1", name = "bash", arguments = { command = "sleep 30 & echo hi" } }, {})
            local jid = res.details and res.details.job_id or ""
            local j = jid ~= "" and J:get(jid) or nil
            local jstatus = j and j.status or ""
            if jid ~= "" then
                pcall(J.cancel, J, jid, "test cleanup")
            end

            local r = mare.proc.run({ cmd = "sh", args = { "-c", "sleep 30 & echo direct-hi" } })
            local bg_job = r.background_job
            local bg_state = bg_job and bg_job:status().state or ""
            if bg_job then
                bg_job:stop()
            end
            return tostring(res.content or ""), jid, jstatus, r.background_left == true, bg_state
            "#,
        )
        .await;
        assert!(t_out.contains("hi"), "bash output missing: {t_out}");
        assert!(!job_id.is_empty(), "expected tracked background job_id in details: {t_out}");
        assert_eq!(job_status, "running", "descendant holding pipes must remain tracked as running job: {t_out}");
        assert!(direct_bg, "mare.proc.run must report background_left=true");
        assert_eq!(direct_state, "running", "mare.proc.run must return running background_job handle");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn persistent_shell_exiting_with_pipe_holding_descendant_is_tracked_immediately() {
    local(async {
        let env = TestEnv::new().await;
        // Default shell_session = "persistent" (no override).
        let h = env.host().builtins(&["tools", "jobs"]).no_session().build().await;
        expose(&h);
        let started = std::time::Instant::now();
        let (t_out, job_id, job_status, after_out): (String, String, String, String) = eval(
            &h,
            r#"
            local T = assert(C.cordis:get("tools"))
            local J = assert(C.cordis:get("jobs"))
            local res = T:execute({
                id = "c-persist-1",
                name = "bash",
                arguments = { command = "sleep 30 & echo persistent-hi; exit", timeout_ms = 8000 },
            }, {})
            local jid = res.details and res.details.job_id or ""
            local j = jid ~= "" and J:get(jid) or nil
            local jstatus = j and j.status or ""
            if jid ~= "" then
                pcall(J.cancel, J, jid, "test cleanup")
            end
            local after = T:execute({
                id = "c-persist-2",
                name = "bash",
                arguments = { command = "echo after-reset" },
            }, {})
            return tostring(res.content or ""), jid, jstatus, tostring(after.content or "")
            "#,
        )
        .await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(4),
            "persistent bash waited for stall/timeout ({elapsed:?}) instead of returning on background_left: {t_out}"
        );
        assert!(t_out.contains("persistent-hi"), "missing output before exit: {t_out}");
        assert!(
            t_out.contains("background processes it started are still running"),
            "expected background_left notice, got: {t_out}"
        );
        assert!(!job_id.is_empty(), "expected tracked background job_id in details: {t_out}");
        assert_eq!(job_status, "running", "descendant holding pipes must remain tracked as running: {t_out}");
        assert!(after_out.contains("after-reset"), "next command in fresh persistent shell failed: {after_out}");
        h.shutdown().await;
    })
    .await
}
