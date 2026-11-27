//! `mare.proc.run`: run a process to completion with streaming output, spill file, timeout,
//! cancellation, and sandbox diagnostics.

use std::process::ExitStatus;
use std::time::Duration;

use mlua::{Function, Lua, Table};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio_util::sync::CancellationToken;

use super::super::headtail::HeadTail;
use super::super::ossandbox::Prepared;
use super::super::outdecode::StreamDecoder;
use super::super::platform::{self, Signal};
use super::super::task::opt_token;
use super::build::{PROTECTION_CLEANUP_GRACE, SpawnOpts, build, parse_opts, spawn_monitored};
use super::{INTERRUPT_WAIT, STOP_GRACE, attach_monitor, checked_spill_path, kill_group, status_table};
use mare_shared::lua::rt_err;

struct RunOpts {
    spawn: SpawnOpts,
    input: Option<mlua::LuaString>,
    timeout: Option<Duration>,
    grace: Duration,
    on_output: Option<Function>,
    max_output: usize,
    spill: Option<String>,
    spill_prefix: Option<String>,
    merge_stderr: bool,
    token: Option<CancellationToken>,
}

struct StreamState {
    out: ChildStdout,
    err: ChildStderr,
    ob: HeadTail,
    eb: HeadTail,
    obuf: Vec<u8>,
    ebuf: Vec<u8>,
    o_done: bool,
    e_done: bool,
    od: StreamDecoder,
    ed: StreamDecoder,
    spill: Option<tokio::fs::File>,
    merge: bool,
}

struct RunOutcome {
    exited: Option<ExitStatus>,
    timed_out: bool,
    cancelled: bool,
    background_left: bool,
}

struct ResultMeta<'a> {
    sandbox: Option<&'a Prepared>,
    timed_out: bool,
    cancelled: bool,
    stop_unconfirmed: bool,
    background_left: bool,
}

fn parse_run_opts(opts: &Table) -> mlua::Result<RunOpts> {
    let o = parse_opts(opts)?;
    let input: Option<mlua::LuaString> = opts.get("input")?;
    let timeout = opts.get::<Option<f64>>("timeout_ms")?.map(|ms| Duration::from_millis(ms as u64));
    // stop: SIGTERM, then SIGKILL after `kill_grace_ms`
    let grace = opts
        .get::<Option<f64>>("kill_grace_ms")?
        .map(|ms| Duration::from_millis(ms.max(0.0) as u64))
        .unwrap_or(STOP_GRACE);
    let on_output: Option<Function> = opts.get("on_output")?;
    let max_output = opts.get::<Option<usize>>("max_output")?.unwrap_or(4 * 1024 * 1024);
    let token = opt_token(&Some(opts.clone()));
    let spill = opts.get::<Option<String>>("spill")?;
    let spill_prefix = opts.get::<Option<String>>("spill_prefix")?;
    let merge_stderr = opts.get::<Option<String>>("stderr")?.as_deref() == Some("merge");
    let spawn = SpawnOpts { stdin: input.is_some(), ..o };
    Ok(RunOpts { spawn, input, timeout, grace, on_output, max_output, spill, spill_prefix, merge_stderr, token })
}

async fn open_spill_file(
    lua: &Lua,
    spill: Option<&str>,
    prefix: Option<&str>,
    call: Option<&Prepared>,
) -> mlua::Result<Option<tokio::fs::File>> {
    let Some(p) = spill else { return Ok(None) };
    let p = checked_spill_path(lua, p, prefix, call.map(|sb| &sb.policy))?;
    if let Some(d) = p.parent() {
        mare_shared::private::create_dir_all(d).map_err(rt_err)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.create(true).write(true).truncate(true);
    let file = mare_shared::private::open(&mut o, &p).map_err(rt_err)?;
    Ok(Some(tokio::fs::File::from_std(file)))
}

async fn emit_stdout_chunk(
    lua: &Lua,
    ectx: &Option<Table>,
    ob: &mut HeadTail,
    spill: &mut Option<tokio::fs::File>,
    on_output: Option<&Function>,
    text: &str,
) -> mlua::Result<()> {
    ob.push(text.as_bytes());
    if let Some(f) = spill.as_mut() {
        let _ = f.write_all(text.as_bytes()).await;
    }
    if let Some(f) = on_output {
        let _ = mare_shared::exec_ctx::call_in::<()>(lua, ectx.clone(), f.clone(), lua.create_string(text)?).await;
    }
    Ok(())
}

async fn stream_output_loop(
    lua: &Lua,
    ectx: &Option<Table>,
    child: &mut Child,
    st: &mut StreamState,
    on_output: Option<&Function>,
    deadline: Option<tokio::time::Instant>,
    cancel: &CancellationTokenOpt,
) -> mlua::Result<RunOutcome> {
    let mut timed_out = false;
    let mut cancelled = false;
    // the shell itself may exit while a background child keeps the pipes open: don't
    // wait for EOF — after a short grace, stop reading and say so
    let mut exited: Option<ExitStatus> = None;
    let mut drain_until: Option<tokio::time::Instant> = None;
    let mut background_left = false;
    let mut prefer_err = false;
    while !(st.o_done && st.e_done) {
        enum PipeRead {
            Out(std::io::Result<usize>),
            Err(std::io::Result<usize>),
        }
        let read_pipes = async {
            if prefer_err {
                tokio::select! {
                    biased;
                    r = st.err.read(&mut st.ebuf), if !st.e_done => PipeRead::Err(r),
                    r = st.out.read(&mut st.obuf), if !st.o_done => PipeRead::Out(r),
                }
            } else {
                tokio::select! {
                    biased;
                    r = st.out.read(&mut st.obuf), if !st.o_done => PipeRead::Out(r),
                    r = st.err.read(&mut st.ebuf), if !st.e_done => PipeRead::Err(r),
                }
            }
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                cancelled = true;
                break;
            }
            _ = sleep_until(deadline) => {
                timed_out = true;
                break;
            }
            s = child.wait(), if exited.is_none() => {
                exited = s.ok();
                drain_until = Some(tokio::time::Instant::now() + Duration::from_millis(250));
            }
            _ = async { tokio::time::sleep_until(drain_until.unwrap()).await }, if drain_until.is_some() => {
                background_left = !(st.o_done && st.e_done);
                break;
            }
            pipe = read_pipes => {
                prefer_err = !prefer_err;
                match pipe {
                    PipeRead::Out(r) => {
                        let n = r.unwrap_or(0);
                        let text = if n == 0 { st.o_done = true; st.od.finish() } else { st.od.push(&st.obuf[..n]) };
                        if text.is_empty() { continue; }
                        emit_stdout_chunk(lua, ectx, &mut st.ob, &mut st.spill, on_output, &text).await?;
                    }
                    PipeRead::Err(r) => {
                        let n = r.unwrap_or(0);
                        let text = if n == 0 { st.e_done = true; st.ed.finish() } else { st.ed.push(&st.ebuf[..n]) };
                        if text.is_empty() { continue; }
                        if st.merge {
                            emit_stdout_chunk(lua, ectx, &mut st.ob, &mut st.spill, on_output, &text).await?;
                        } else {
                            st.eb.push(text.as_bytes());
                        }
                    }
                }
            }
        }
    }
    // Both streams may close while the process keeps running (e.g. `exec >&-`): the wait
    // for exit must still honour the deadline and the cancellation token.
    if !(timed_out || cancelled) && exited.is_none() {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => cancelled = true,
            s = child.wait() => exited = s.ok(),
            _ = sleep_until(deadline) => timed_out = true,
        }
    }
    Ok(RunOutcome { exited, timed_out, cancelled, background_left })
}

async fn stop_or_wait_child(
    child: &mut Child,
    pid: Option<u32>,
    grace: Duration,
    outcome: &RunOutcome,
) -> mlua::Result<(Option<ExitStatus>, bool)> {
    let mut stop_unconfirmed = false;
    // registered while it lasts: a second Ctrl+C SIGKILLs the tree at once
    let _stopping = (outcome.timed_out || outcome.cancelled).then(|| super::super::stopping::Guard::new(pid));
    let st = if outcome.timed_out || outcome.cancelled {
        // an interrupt is an interrupt first (deliver SIGINT before the hard stop):
        // send SIGINT to the group and wait a moment for it to exit
        let mut early = None;
        if outcome.cancelled {
            kill_group(pid, Signal::Int);
            early = tokio::time::timeout(INTERRUPT_WAIT, child.wait()).await.ok().and_then(|r| r.ok());
        }
        if early.is_some() && !platform::tree_alive(pid) {
            early
        } else {
            let (st, confirmed) = super::stop_tree(child, pid, grace).await;
            stop_unconfirmed = !confirmed;
            st
        }
    } else if let Some(s) = outcome.exited {
        Some(s)
    } else {
        Some(child.wait().await.map_err(rt_err)?)
    };
    Ok((st, stop_unconfirmed))
}

async fn drain_after_stop(st: &mut StreamState) {
    // what the command wrote while it was being stopped (an INT handler's last words)
    // is still in the pipes: drain it, bounded
    let until = tokio::time::Instant::now() + Duration::from_millis(300);
    while !(st.o_done && st.e_done) {
        tokio::select! {
            r = st.out.read(&mut st.obuf), if !st.o_done => {
                let n = r.unwrap_or(0);
                let text = if n == 0 { st.o_done = true; st.od.finish() } else { st.od.push(&st.obuf[..n]) };
                st.ob.push(text.as_bytes());
                if let Some(f) = st.spill.as_mut() {
                    let _ = f.write_all(text.as_bytes()).await;
                }
            }
            r = st.err.read(&mut st.ebuf), if !st.e_done => {
                let n = r.unwrap_or(0);
                let text = if n == 0 { st.e_done = true; st.ed.finish() } else { st.ed.push(&st.ebuf[..n]) };
                if st.merge { st.ob.push(text.as_bytes()) } else { st.eb.push(text.as_bytes()) }
            }
            _ = tokio::time::sleep_until(until) => break,
        }
    }
}

fn build_run_result(
    lua: &Lua,
    st: Option<ExitStatus>,
    ob: HeadTail,
    eb: HeadTail,
    meta: ResultMeta<'_>,
) -> mlua::Result<Table> {
    // not reaped: report honestly instead of blocking forever on wait()
    let Some(st) = st else {
        let t = lua.create_table()?;
        t.set("code", mlua::Value::Nil)?;
        t.set("stdout", lua.create_string(ob.finish())?)?;
        t.set("stderr", lua.create_string(eb.finish())?)?;
        t.set("timed_out", meta.timed_out)?;
        t.set("cancelled", meta.cancelled)?;
        t.set("stop_unconfirmed", true)?;
        return Ok(t);
    };
    let dropped = ob.omitted() + eb.omitted();
    let (ob, eb) = (ob.finish(), eb.finish());
    let t = status_table(lua, st)?;
    t.set("stdout", lua.create_string(&ob)?)?;
    t.set("stderr", lua.create_string(&eb)?)?;
    t.set("timed_out", meta.timed_out)?;
    t.set("cancelled", meta.cancelled)?;
    if meta.stop_unconfirmed {
        t.set("stop_unconfirmed", true)?;
    }
    t.set("dropped", dropped)?;
    if meta.background_left {
        t.set("background_left", true)?;
    }
    if let Some(sb) = meta.sandbox {
        attach_sandbox_diagnostics(lua, &t, sb, st, &ob, &eb)?;
    }
    Ok(t)
}

fn attach_sandbox_diagnostics(
    lua: &Lua,
    t: &Table,
    sb: &Prepared,
    st: ExitStatus,
    ob: &[u8],
    eb: &[u8],
) -> mlua::Result<()> {
    t.set("sandbox", sb.report(lua)?)?;
    let stderr_text = String::from_utf8_lossy(eb);
    if let Some(detail) = sb.start_failure(st.code(), &stderr_text) {
        // the sandbox itself failed to start: not a denial of the command
        t.set("sandbox_failed", detail.clone())?;
        t.set("sandbox_marker", format!("[sandbox: failed to start ({}): {detail}]", sb.backend.name()))?;
    } else if !st.success() || sb.proxy.as_ref().is_some_and(|p| !p.denied().is_empty()) {
        let text = format!("{stderr_text}\n{}", String::from_utf8_lossy(ob));
        if let Some(d) = sb.classify(&text) {
            let dt = d.to_lua(lua)?;
            dt.set("marker", sb.policy.denial_marker(&d))?;
            t.set("sandbox_denied", d.kind)?;
            t.set("sandbox_marker", sb.policy.denial_marker(&d))?;
            t.set("denial", dt)?;
        }
    }
    Ok(())
}

async fn execute_run(lua: Lua, opts: Table, cwd: String) -> mlua::Result<Table> {
    let ectx = mare_shared::exec_ctx::current(&lua);
    let ro = parse_run_opts(&opts)?;
    let mut cmd = build(&lua, &ro.spawn, &cwd)?;
    let sandbox = ro.spawn.prepared.take();
    let spill = open_spill_file(&lua, ro.spill.as_deref(), ro.spill_prefix.as_deref(), sandbox.as_ref()).await?;
    let (mut child, monitor) = spawn_monitored(&mut cmd, sandbox.as_ref(), "spawn")?;
    let pid = child.id();
    // registered until the tree is gone: a run abandoned midway (its task aborted,
    // the runtime shutting down) ends the whole tree, not only the root
    let tree = super::super::trees::Tree::new(pid, Some(super::super::trees::Owner::of(&lua)));
    let monitor = attach_monitor(monitor, &mut child)?;
    if let (Some(inp), Some(mut w)) = (ro.input, child.stdin.take()) {
        let data = inp.as_bytes().to_vec();
        tokio::spawn(async move {
            let _ = w.write_all(&data).await;
        });
    }
    // head + tail are kept, the middle is dropped (first errors survive long output)
    // bytes → text per stream, characters split across reads kept whole; non-UTF-8
    // output decoded (Windows console code page / encoding detection) via output_decode
    let mut streams = StreamState {
        out: child.stdout.take().unwrap(),
        err: child.stderr.take().unwrap(),
        ob: HeadTail::new(ro.max_output),
        eb: HeadTail::new(ro.max_output),
        obuf: vec![0u8; 16384],
        ebuf: vec![0u8; 16384],
        o_done: false,
        e_done: false,
        od: StreamDecoder::new(),
        ed: StreamDecoder::new(),
        spill,
        merge: ro.merge_stderr,
    };
    let deadline = ro.timeout.map(|d| tokio::time::Instant::now() + d);
    let cancel = CancellationTokenOpt(ro.token);
    let outcome =
        stream_output_loop(&lua, &ectx, &mut child, &mut streams, ro.on_output.as_ref(), deadline, &cancel).await?;
    // hard stop: SIGTERM → grace → SIGKILL → bounded reap
    let (st, stop_unconfirmed) = stop_or_wait_child(&mut child, pid, ro.grace, &outcome).await?;
    if outcome.timed_out || outcome.cancelled {
        drain_after_stop(&mut streams).await;
    }
    if let Some(f) = streams.spill.as_mut() {
        let _ = f.flush().await;
    }
    // Prefer completing ordinary cleanup before returning, but never tie a finished
    // command to procfs availability. On timeout the independent monitor still owns
    // the lease and will remove the target only after it can verify the group is gone.
    if !stop_unconfirmed && let Some(monitor) = monitor {
        let _ = tokio::time::timeout(PROTECTION_CLEANUP_GRACE, monitor.finished()).await;
    }
    // what the command left running in the background still ends with the host
    let meta = ResultMeta {
        sandbox: sandbox.as_ref(),
        timed_out: outcome.timed_out,
        cancelled: outcome.cancelled,
        stop_unconfirmed,
        background_left: outcome.background_left,
    };
    let result = build_run_result(&lua, st, streams.ob, streams.eb, meta)?;
    if outcome.background_left {
        let bg_job = super::super::job::from_lingering(
            pid,
            tree,
            st,
            (!streams.o_done).then_some(streams.out),
            (!streams.e_done).then_some(streams.err),
            streams.od,
            streams.ed,
            streams.spill,
            sandbox,
        );
        result.set("background_job", bg_job)?;
    } else {
        tree.linger();
    }
    Ok(result)
}

pub(super) fn register_run(lua: &Lua, t: &Table, cwd: String) -> mlua::Result<()> {
    // run{cmd, args, cwd, env, input, timeout_ms, kill_grace_ms, cancel, on_output=fn(chunk), max_output}
    //   -> {code, signal, stdout, stderr, timed_out, cancelled, stop_unconfirmed?, dropped}
    t.set(
        "run",
        lua.create_async_function(move |lua, opts: Table| {
            let cwd = cwd.clone();
            async move { execute_run(lua, opts, cwd).await }
        })?,
    )
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

struct CancellationTokenOpt(Option<CancellationToken>);
impl CancellationTokenOpt {
    async fn cancelled(&self) {
        match &self.0 {
            Some(t) => t.cancelled().await,
            None => std::future::pending().await,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::fd::{AsRawFd, RawFd};

    use super::*;

    /// Wait via `poll(2)` until `fd` has buffered bytes ready to read (without consuming them).
    fn wait_pipe_readable(fd: RawFd) {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: `pfd` points to a single valid `pollfd` for the live child pipe fd.
        let rc = unsafe { libc::poll(&mut pfd, 1, 2000) };
        assert!(rc > 0 && (pfd.revents & libc::POLLIN) != 0, "pipe did not become readable (rc={rc})");
    }

    #[tokio::test]
    async fn stream_output_loop_prefers_cancel_when_deadline_is_already_elapsed() {
        let lua = Lua::new();
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "sleep 30"]).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let mut st = StreamState {
            out,
            err,
            ob: HeadTail::new(4096),
            eb: HeadTail::new(4096),
            obuf: vec![0u8; 256],
            ebuf: vec![0u8; 256],
            o_done: false,
            e_done: false,
            od: StreamDecoder::new(),
            ed: StreamDecoder::new(),
            spill: None,
            merge: true,
        };
        let past = Some(tokio::time::Instant::now() - Duration::from_secs(1));
        let tok = CancellationToken::new();
        tok.cancel();
        let cancel = CancellationTokenOpt(Some(tok));
        for i in 0..30 {
            let outcome = stream_output_loop(&lua, &None, &mut child, &mut st, None, past, &cancel).await.unwrap();
            assert!(
                outcome.cancelled && !outcome.timed_out,
                "iteration {i}: expected cancelled=true, timed_out=false; got cancelled={}, timed_out={}",
                outcome.cancelled,
                outcome.timed_out
            );
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn stream_output_loop_honours_deadline_and_stderr_under_sustained_stdout() {
        let lua = Lua::new();
        let cancel = CancellationTokenOpt(None);

        // Part 1: an already-elapsed deadline must take priority over a ready stdout pipe.
        let mut cmd0 = tokio::process::Command::new("sh");
        cmd0.args(["-c", "printf 'should-not-starve-deadline\\n'; sleep 30"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child0 = cmd0.spawn().unwrap();
        let out0 = child0.stdout.take().unwrap();
        let err0 = child0.stderr.take().unwrap();
        let mut st0 = StreamState {
            out: out0,
            err: err0,
            ob: HeadTail::new(4096),
            eb: HeadTail::new(4096),
            obuf: vec![0u8; 64],
            ebuf: vec![0u8; 64],
            o_done: false,
            e_done: false,
            od: StreamDecoder::new(),
            ed: StreamDecoder::new(),
            spill: None,
            merge: false,
        };
        wait_pipe_readable(st0.out.as_raw_fd());
        let past = Some(tokio::time::Instant::now() - Duration::from_secs(1));
        let outcome0 = stream_output_loop(&lua, &None, &mut child0, &mut st0, None, past, &cancel).await.unwrap();
        let _ = child0.kill().await;
        let _ = child0.wait().await;
        assert!(
            outcome0.timed_out && !outcome0.cancelled,
            "expected timed_out=true, got timed_out={}, cancelled={}",
            outcome0.timed_out,
            outcome0.cancelled
        );
        assert!(st0.ob.finish().is_empty(), "ready stdout pipe must not win over an already-elapsed deadline");

        // Part 2: sustained stdout must not starve deadline or concurrent stderr reads.
        let mut cmd2 = tokio::process::Command::new("sh");
        cmd2.args(["-c", "yes ERR-LINE >&2 & p=$!; trap 'kill $p 2>/dev/null' EXIT; yes OUT-FLOOD-LINE"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child2 = cmd2.spawn().unwrap();
        let out2 = child2.stdout.take().unwrap();
        let err2 = child2.stderr.take().unwrap();
        let mut st2 = StreamState {
            out: out2,
            err: err2,
            ob: HeadTail::new(4096),
            eb: HeadTail::new(4096),
            obuf: vec![0u8; 64],
            ebuf: vec![0u8; 64],
            o_done: false,
            e_done: false,
            od: StreamDecoder::new(),
            ed: StreamDecoder::new(),
            spill: None,
            merge: false,
        };
        wait_pipe_readable(st2.out.as_raw_fd());
        wait_pipe_readable(st2.err.as_raw_fd());
        let deadline2 = Some(tokio::time::Instant::now() + Duration::from_millis(120));
        let outcome2 = tokio::time::timeout(
            Duration::from_secs(2),
            stream_output_loop(&lua, &None, &mut child2, &mut st2, None, deadline2, &cancel),
        )
        .await
        .expect("deadline was starved by sustained dual-pipe flood")
        .unwrap();
        let _ = child2.kill().await;
        let _ = child2.wait().await;
        assert!(
            outcome2.timed_out && !outcome2.cancelled,
            "expected timed_out=true, got timed_out={}, cancelled={}",
            outcome2.timed_out,
            outcome2.cancelled
        );
        assert!(!st2.ob.finish().is_empty(), "stdout should have been read");
        assert!(!st2.eb.finish().is_empty(), "stderr was starved by sustained stdout");
    }
}
