//! `mare.proc.start`: long-running processes whose output lands in a bounded ring buffer in Rust
//! . Output never streams into Lua chunk by chunk: Lua reads it with
//! a consuming cursor (`read`, for the model) or by absolute offset (`read_at` / `tail`, for the UI).
//! Every chunk records its absolute byte offset; bytes pushed out of the ring are reported as
//! `dropped` by the next read that wanted them.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use mlua::{Lua, Table, UserData, UserDataMethods};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::{Mutex as AsyncMutex, watch};

use super::platform::{self, ProcessBackend, Signal};
use super::proc::{build, kill_group, parse_opts, signal_num};
use super::task::opt_token;
use super::text::truncate::{split_utf8_boundary, trailing_incomplete_utf8_len};
use mare_shared::lua::rt_err;

const DEFAULT_CAP: usize = 1024 * 1024;
const DEFAULT_READ: usize = 50 * 1024;
/// Small writes are merged into the previous chunk of the same stream up to this size.
const MERGE: usize = 8 * 1024;
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(20);
const EXIT_GRACE_PERIOD: Duration = Duration::from_millis(250);
const TREE_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Out,
    Err,
}

struct Chunk {
    stream: Stream,
    /// absolute offset of `data[0]` in the job's combined output
    off: u64,
    data: Vec<u8>,
}

/// Bounded output of a job: the first half of the cap is a frozen head (the beginning of the
/// output is kept — first errors survive), the rest is a ring holding the newest bytes; the middle
/// is dropped. Every chunk keeps its absolute offset.
struct Ring {
    head: Vec<Chunk>,
    head_cap: usize,
    head_size: usize,
    chunks: VecDeque<Chunk>,
    cap: usize,
    size: usize,
    /// total bytes ever written (= offset one past the last byte)
    total: u64,
}

fn push_merged(chunks: &mut VecDeque<Chunk>, stream: Stream, off: u64, data: &[u8]) {
    match chunks.back_mut() {
        Some(last) if last.stream == stream && last.data.len() + data.len() <= MERGE => {
            last.data.extend_from_slice(data)
        }
        _ => chunks.push_back(Chunk { stream, off, data: data.to_vec() }),
    }
}

impl Ring {
    fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        let head_cap = cap / 2;
        Self {
            head: Vec::new(),
            head_cap,
            head_size: 0,
            chunks: VecDeque::new(),
            cap: cap - head_cap,
            size: 0,
            total: 0,
        }
    }

    fn drain_front_bytes(&mut self, mut n: usize, tail_buf: &mut Vec<u8>) {
        while n > 0 {
            let Some(front) = self.chunks.front_mut() else {
                break;
            };
            if front.data.len() <= n {
                let take = front.data.len();
                let keep_start = take.saturating_sub(3);
                tail_buf.extend_from_slice(&front.data[keep_start..]);
                if tail_buf.len() > 3 {
                    tail_buf.drain(..tail_buf.len() - 3);
                }
                self.size -= take;
                n -= take;
                self.chunks.pop_front();
            } else {
                let keep_start = n.saturating_sub(3);
                tail_buf.extend_from_slice(&front.data[keep_start..n]);
                if tail_buf.len() > 3 {
                    tail_buf.drain(..tail_buf.len() - 3);
                }
                front.data.drain(..n);
                front.off += n as u64;
                self.size -= n;
                n = 0;
            }
        }
    }

    fn peek_front_bytes(&self, max: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(max);
        for c in &self.chunks {
            for &b in &c.data {
                out.push(b);
                if out.len() == max {
                    return out;
                }
            }
        }
        out
    }

    fn push(&mut self, stream: Stream, mut data: &[u8]) {
        let room = self.head_cap - self.head_size;
        if room > 0 {
            let n = room.min(data.len());
            match self.head.last_mut() {
                Some(last) if last.stream == stream => last.data.extend_from_slice(&data[..n]),
                _ => self.head.push(Chunk { stream, off: self.total, data: data[..n].to_vec() }),
            }
            self.head_size += n;
            self.total += n as u64;
            data = &data[n..];
            if data.is_empty() {
                return;
            }
        }
        push_merged(&mut self.chunks, stream, self.total, data);
        self.total += data.len() as u64;
        self.size += data.len();
        if self.size > self.cap {
            let over = self.size - self.cap;
            let mut dropped_tail = Vec::with_capacity(6);
            if self.chunks.front().is_some_and(|f| f.off == self.head_size as u64) {
                dropped_tail.extend_from_slice(&self.peek_bytes_before(self.head_size as u64, 3));
            }
            self.drain_front_bytes(over, &mut dropped_tail);
            let next = self.peek_front_bytes(3);
            if let Some((_, cont)) = split_utf8_boundary(&dropped_tail, &next) {
                let mut ignore = Vec::new();
                self.drain_front_bytes(cont, &mut ignore);
            }
        }
    }

    /// Peek up to `max` contiguous bytes starting at `offset` across buffered chunks.
    /// Stops at any ring eviction gap (`c.off > cur`).
    fn peek_bytes_at(&self, mut offset: u64, max: usize, only: Option<Stream>) -> Vec<u8> {
        let mut out = Vec::with_capacity(max);
        for c in self.head.iter().chain(self.chunks.iter()) {
            let end = c.off + c.data.len() as u64;
            if end <= offset {
                continue;
            }
            if c.off > offset {
                break;
            }
            let skip = (offset - c.off) as usize;
            if only.is_none_or(|s| s == c.stream) {
                let take = (c.data.len() - skip).min(max - out.len());
                out.extend_from_slice(&c.data[skip..skip + take]);
                if out.len() == max {
                    break;
                }
            }
            offset = end;
        }
        out
    }

    /// Peek up to `max` contiguous bytes immediately ending at `offset` across buffered chunks.
    fn peek_bytes_before(&self, offset: u64, max: usize) -> Vec<u8> {
        let start = offset.saturating_sub(max as u64);
        let mut cur = start;
        let mut out = Vec::with_capacity(max);
        for c in self.head.iter().chain(self.chunks.iter()) {
            let end = c.off + c.data.len() as u64;
            if end <= cur {
                continue;
            }
            if c.off >= offset {
                break;
            }
            if c.off > cur {
                out.clear();
                cur = c.off;
            }
            let skip = (cur - c.off) as usize;
            let stop = (offset.min(end) - c.off) as usize;
            out.extend_from_slice(&c.data[skip..stop]);
            cur = c.off + stop as u64;
        }
        if cur < offset {
            out.clear();
        }
        out
    }

    /// Last `n` bytes of the ring, aligned forward off any split valid multi-byte UTF-8 character
    /// while preserving genuinely invalid raw bytes.
    fn tail(&self, n: usize) -> Vec<u8> {
        if n == 0 || self.total == 0 {
            return Vec::new();
        }
        let mut from = self.total.saturating_sub(n as u64);
        let mut limit = n;
        if from > 0 {
            let prev = self.peek_bytes_before(from, 3);
            let next = self.peek_bytes_at(from, 3, None);
            if let Some((_, cont)) = split_utf8_boundary(&prev, &next) {
                from += cont as u64;
                limit = limit.saturating_sub(cont);
            }
        }
        self.read(from, limit, None).0
    }

    /// Bytes from `offset`, at most `max`, optionally one stream only.
    /// -> (bytes, next offset, bytes lost). Bytes lost before the first returned byte are only
    /// counted; a gap crossed after some data (head → tail) is also marked inline.
    fn read(&self, offset: u64, max: usize, only: Option<Stream>) -> (Vec<u8>, u64, u64) {
        if max == 0 {
            return (Vec::new(), offset, 0);
        }
        let mut out = Vec::new();
        let mut last_offs: Vec<u64> = Vec::with_capacity(4);
        let mut from = offset;
        let mut dropped = 0u64;
        for c in self.head.iter().chain(self.chunks.iter()) {
            let end = c.off + c.data.len() as u64;
            if end <= from {
                continue;
            }
            if c.off > from {
                let mut gap = c.off - from;
                if !out.is_empty() {
                    let trim = trailing_incomplete_utf8_len(&out);
                    if trim > 0 {
                        out.truncate(out.len() - trim);
                        gap += trim as u64;
                    }
                    if !out.is_empty() {
                        out.extend_from_slice(super::headtail::marker(gap as usize).as_bytes());
                    }
                    last_offs.clear();
                }
                dropped += gap;
                from = c.off;
            }
            let skip = (from - c.off) as usize;
            if only.is_none_or(|s| s == c.stream) {
                let avail = c.data.len() - skip;
                let remaining = max.saturating_sub(out.len());
                let take = avail.min(remaining);
                let base_off = c.off + skip as u64;
                out.extend_from_slice(&c.data[skip..skip + take]);
                for idx in take.saturating_sub(3)..take {
                    last_offs.push(base_off + idx as u64);
                }
                if last_offs.len() > 3 {
                    last_offs.drain(..last_offs.len() - 3);
                }
                from = base_off + take as u64;
                if out.len() >= max {
                    let after = self.peek_bytes_at(from, 3, only);
                    let tail_start = out.len().saturating_sub(3);
                    if let Some((pre, _)) = split_utf8_boundary(&out[tail_start..], &after)
                        && pre <= last_offs.len()
                    {
                        let rewind_off = last_offs[last_offs.len() - pre];
                        out.truncate(out.len() - pre);
                        from = rewind_off;
                    }
                    break;
                }
            } else {
                from = end;
            }
        }
        (out, from, dropped)
    }
}

#[derive(Clone, Default)]
struct Exit {
    code: Option<i32>,
    signal: Option<i32>,
}

struct State {
    ring: Ring,
    cursor: u64,
    exit: Option<Exit>,
    ended: Option<Instant>,
    /// a hard stop couldn't confirm the tree ended: never reuse
    stop_unconfirmed: bool,
    /// the shell exited while descendants still hold stdout/stderr pipes open
    background_left: bool,
}

enum Input {
    Pipe(AsyncMutex<Option<ChildStdin>>),
    Pty(Arc<super::pty::Pty>),
}

pub struct Job {
    pid: Option<u32>,
    started: Instant,
    state: Arc<Mutex<State>>,
    exited: watch::Receiver<bool>,
    input: Input,
    /// sandbox + network proxy: must live as long as the job (the proxy is the only reachable
    /// endpoint for filtered network; dropping it fails closed and loses denial records)
    sandbox: Option<super::ossandbox::Prepared>,
    /// Policy fence and generation check for persistent writable stdin pipe jobs (PTY jobs hold
    /// their own lease inside `Pty`).
    lease: Option<super::pty::lease::Lease>,
}

impl Drop for Job {
    fn drop(&mut self) {
        // a job nobody holds any more can't be read or stopped: end it
        if self.state.lock().unwrap().exit.is_none() {
            if let Input::Pty(pty) = &self.input {
                let _ = pty.kill();
            }
            kill_group(self.pid, Signal::Kill);
        }
    }
}

fn stream_arg(s: Option<String>) -> Option<Stream> {
    match s.as_deref() {
        Some("stdout") => Some(Stream::Out),
        Some("stderr") => Some(Stream::Err),
        _ => None,
    }
}

impl Job {
    fn sandbox_prepared(&self) -> Option<&super::ossandbox::Prepared> {
        match &self.input {
            Input::Pipe(_) => self.sandbox.as_ref(),
            Input::Pty(pty) => pty.sandbox_prepared(),
        }
    }

    fn status(&self, lua: &Lua) -> mlua::Result<Table> {
        let st = self.state.lock().unwrap();
        let t = lua.create_table()?;
        if st.stop_unconfirmed {
            t.set("stop_unconfirmed", true)?;
        }
        if st.background_left {
            t.set("background_left", true)?;
        }
        match &st.exit {
            None => t.set("state", "running")?,
            Some(e) => {
                t.set("state", "exited")?;
                t.set("code", e.code)?;
                t.set("signal", e.signal)?;
                t.set("success", e.code == Some(0))?;
            }
        }
        let end = st.ended.unwrap_or_else(Instant::now);
        t.set("elapsed_ms", end.duration_since(self.started).as_millis() as u64)?;
        t.set("bytes", st.ring.total)?;
        t.set("unread", st.ring.total - st.cursor.min(st.ring.total))?;
        t.set("pid", self.pid)?;
        Ok(t)
    }
}

impl UserData for Job {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("pid", |_, this, ()| Ok(this.pid));
        m.add_method("status", |lua, this, ()| this.status(lua));
        // sandbox() -> what was actually enforced; sandbox_denied(output?) -> real refusals only
        m.add_method("sandbox", |lua, this, ()| this.sandbox_prepared().map(|p| p.report(lua)).transpose());
        m.add_method("sandbox_denied", |lua, this, output: Option<String>| {
            let Some(p) = this.sandbox_prepared() else { return Ok(None) };
            p.classify(output.as_deref().unwrap_or("")).map(|d| d.to_lua(lua)).transpose()
        });
        // read{max_bytes?, stream?} -> {text, dropped, offset, done}: everything after the cursor
        // (consumed); `done` once the process exited and all output was read
        m.add_method("read", |lua, this, o: Option<Table>| {
            let max =
                o.as_ref().and_then(|o| o.get::<Option<usize>>("max_bytes").ok().flatten()).unwrap_or(DEFAULT_READ);
            let only = stream_arg(o.as_ref().and_then(|o| o.get("stream").ok().flatten()));
            let mut st = this.state.lock().unwrap();
            let (data, next, dropped) = st.ring.read(st.cursor, max, only);
            let offset = st.cursor + dropped;
            st.cursor = next;
            let t = lua.create_table()?;
            t.set("text", lua.create_string(&data)?)?;
            t.set("dropped", dropped)?;
            t.set("offset", offset)?;
            t.set("done", st.exit.is_some() && st.cursor >= st.ring.total)?;
            Ok(t)
        });
        // read_at(offset, {max_bytes?, stream?}) -> {text, next, dropped}: does not move the cursor
        m.add_method("read_at", |lua, this, (offset, o): (u64, Option<Table>)| {
            let max =
                o.as_ref().and_then(|o| o.get::<Option<usize>>("max_bytes").ok().flatten()).unwrap_or(DEFAULT_READ);
            let only = stream_arg(o.as_ref().and_then(|o| o.get("stream").ok().flatten()));
            let st = this.state.lock().unwrap();
            let (data, next, dropped) = st.ring.read(offset, max, only);
            let t = lua.create_table()?;
            t.set("text", lua.create_string(&data)?)?;
            t.set("next", next)?;
            t.set("dropped", dropped)?;
            Ok(t)
        });
        // tail(n) -> last n bytes (live view)
        m.add_method("tail", |lua, this, n: Option<usize>| {
            let n = n.unwrap_or(4096);
            let st = this.state.lock().unwrap();
            let data = st.ring.tail(n);
            lua.create_string(&data)
        });
        // wait{timeout_ms?, cancel?} -> status table + timed_out / cancelled
        m.add_async_method("wait", |lua, this, o: Option<Table>| async move {
            let timeout = o
                .as_ref()
                .and_then(|o| o.get::<Option<f64>>("timeout_ms").ok().flatten())
                .map(|ms| Duration::from_millis(ms.max(0.0) as u64));
            let token = opt_token(&o);
            let mut rx = this.exited.clone();
            let done = async {
                let _ = rx.wait_for(|v| *v).await;
            };
            let mut timed_out = false;
            let mut cancelled = false;
            tokio::select! {
                biased;
                _ = done => {}
                _ = async {
                    match &token {
                        Some(t) => t.cancelled().await,
                        None => std::future::pending().await,
                    }
                } => cancelled = true,
                _ = async {
                    match timeout {
                        Some(d) => tokio::time::sleep(d).await,
                        None => std::future::pending().await,
                    }
                } => timed_out = true,
            }
            let t = this.status(&lua)?;
            t.set("timed_out", timed_out)?;
            t.set("cancelled", cancelled)?;
            Ok(t)
        });
        // stop{grace_ms?, interrupt?} -> {stopped, stop_unconfirmed} — hard stop ( part
        // five): SIGTERM the group, wait grace_ms (default 3000) for the exit, SIGKILL / end the
        // job, then wait (bounded) for the reap and for the rest of the group. Unconfirmed →
        // stop_unconfirmed. `interrupt`: a user's interrupt, SIGINT to the
        // group and a moment to stop first, like `run`'s cancellation.
        m.add_async_method("stop", |lua, this, opts: Option<Table>| async move {
            let grace = opts
                .as_ref()
                .and_then(|o| o.get::<Option<f64>>("grace_ms").ok().flatten())
                .map(|ms| Duration::from_millis(ms.max(0.0) as u64))
                .unwrap_or(super::proc::STOP_GRACE);
            let interrupt =
                opts.as_ref().and_then(|o| o.get::<Option<bool>>("interrupt").ok().flatten()).unwrap_or(false);
            let rx = this.exited.clone();
            let wait_exit = |d: Duration| {
                let mut rx = rx.clone();
                async move { tokio::time::timeout(d, rx.wait_for(|v| *v)).await.is_ok() }
            };
            // registered while it lasts: a second Ctrl+C SIGKILLs the tree at once
            let _stopping = super::stopping::Guard::new(this.pid);
            if interrupt {
                if let Input::Pty(pty) = &this.input {
                    let _ = pty.write(b"\x03");
                }
                kill_group(this.pid, Signal::Int);
                if wait_exit(super::proc::INTERRUPT_WAIT).await && !crate::platform::tree_alive(this.pid) {
                    let t = lua.create_table()?;
                    t.set("stopped", true)?;
                    t.set("stop_unconfirmed", false)?;
                    return Ok(t);
                }
            }
            kill_group(this.pid, Signal::Term);
            let mut exited = wait_exit(grace).await;
            if !exited || crate::platform::tree_alive(this.pid) {
                if let Input::Pty(pty) = &this.input {
                    let _ = pty.kill();
                }
                kill_group(this.pid, Signal::Kill);
            }
            if !exited {
                exited = wait_exit(super::proc::REAP_TIMEOUT).await;
            }
            let deadline = tokio::time::Instant::now() + super::proc::REAP_TIMEOUT;
            while crate::platform::tree_alive(this.pid) && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(REAP_POLL_INTERVAL).await;
            }
            let confirmed = exited && !crate::platform::tree_alive(this.pid);
            if !confirmed {
                this.state.lock().unwrap().stop_unconfirmed = true;
            }
            let t = lua.create_table()?;
            t.set("stopped", confirmed)?;
            t.set("stop_unconfirmed", !confirmed)?;
            Ok(t)
        });
        // kill(signal?) — the whole process group; status turns "exited" once it really stopped
        m.add_method("kill", |_, this, sig: Option<String>| {
            if let Input::Pty(pty) = &this.input
                && matches!(sig.as_deref(), Some("KILL"))
            {
                let _ = pty.kill();
            }
            kill_group(this.pid, signal_num(sig.as_deref().unwrap_or("TERM")));
            Ok(())
        });
        m.add_async_method("write", |lua, this, (data, opts): (mlua::LuaString, Option<Table>)| async move {
            crate::exec_lifetime::check_mutation(&lua, "proc write")?;
            let close_stdin =
                opts.as_ref().and_then(|o| o.get::<Option<bool>>("close_stdin").ok().flatten()).unwrap_or(false);
            if this.state.lock().unwrap().exit.is_some() {
                return Err(rt_err("process has exited"));
            }
            match &this.input {
                Input::Pipe(stdin) => {
                    if let Some(lease) = &this.lease {
                        lease.check(&lua)?;
                    }
                    let mut g = stdin.lock().await;
                    let bytes = data.as_bytes();
                    if !bytes.is_empty() || !close_stdin {
                        let w = g.as_mut().ok_or_else(|| rt_err("stdin is closed"))?;
                        if !bytes.is_empty() {
                            w.write_all(&bytes).await.map_err(rt_err)?;
                            w.flush().await.map_err(rt_err)?;
                        }
                    }
                    if close_stdin {
                        g.take();
                    }
                    Ok(())
                }
                Input::Pty(pty) => {
                    pty.check_lease(&lua)?;
                    if close_stdin {
                        return Err(rt_err("cannot close stdin on a PTY job"));
                    }
                    let bytes = data.as_bytes();
                    if !bytes.is_empty() {
                        pty.write(&bytes)?;
                    }
                    Ok(())
                }
            }
        });
        m.add_async_method("close_stdin", |_, this, ()| async move {
            match &this.input {
                Input::Pipe(stdin) => {
                    stdin.lock().await.take();
                    Ok(())
                }
                Input::Pty(_) => Err(rt_err("cannot close stdin on a PTY job")),
            }
        });
    }
}

/// start{cmd, args, cwd, env, clear_env, stdin, input, tty, cols, rows, sandbox, buffer_bytes, spill} -> Job
/// `spill`: a file that receives the complete output (both streams) while the ring keeps the tail.
/// `input`: written to stdin, which is then closed (e.g. the command for a shell that reads its
/// script from stdin, like the legacy WSL launcher).
fn start(lua: &Lua, opts: &Table, cwd: &str) -> mlua::Result<Job> {
    let mut o = parse_opts(opts)?;
    let input: Option<mlua::LuaString> = opts.get("input")?;
    if input.is_some() {
        o.stdin = true;
    }
    let tty = opts.get::<Option<bool>>("tty")?.unwrap_or(false);
    let cols = opts.get::<Option<u16>>("cols")?.unwrap_or(80);
    let rows = opts.get::<Option<u16>>("rows")?.unwrap_or(24);
    let cap = opts.get::<Option<usize>>("buffer_bytes")?.unwrap_or(DEFAULT_CAP);
    let spill: Option<String> = opts.get("spill")?;
    let spill_prefix: Option<String> = opts.get("spill_prefix")?;
    let policy_behavior = match opts.get::<Option<String>>("on_policy_change")?.as_deref() {
        None | Some("fence") => super::pty::lease::PolicyBehavior::Fence,
        Some("stop") => super::pty::lease::PolicyBehavior::AutoStop,
        Some(other) => {
            return Err(rt_err(format!("on_policy_change must be \"fence\" or \"stop\", got {other:?}")));
        }
    };
    if tty {
        let lease = super::pty::lease::Lease::prepare_with(lua, policy_behavior)?;
        return start_pty(lua, &o, cwd, input, cap, spill.as_deref(), spill_prefix.as_deref(), cols, rows, lease);
    }
    // Persistent writable stdin pipes hold launch sandbox authority across calls just like PTYs.
    let mut lease = if o.stdin && input.is_none() {
        Some(super::pty::lease::Lease::prepare_with(lua, policy_behavior)?)
    } else {
        None
    };
    let mut cmd = build(lua, &o, cwd)?;
    let sandbox = o.prepared.take();
    let spill = open_spill(lua, spill.as_deref(), spill_prefix.as_deref(), sandbox.as_ref())?;
    let monitor = sandbox.as_ref().map(|sb| sb.start_monitor()).transpose()?.flatten();
    let mut child = match super::platform::spawn(&mut cmd) {
        Ok(child) => child,
        Err(e) => {
            if let Some(monitor) = &monitor {
                monitor.after_kill();
            }
            return Err(rt_err(format!("spawn: {e}")));
        }
    };
    let pid = child.id();
    if let Some(lease) = lease.as_mut() {
        lease.publish(pid);
        lease.register(lua);
    }
    // registered from the start: ends with the host, the runtime (this task) or mare
    let tree = super::trees::Tree::new(pid, Some(super::trees::Owner::of(lua)));
    let _ = super::proc::attach_monitor(monitor, &mut child)?;
    let state = Arc::new(Mutex::new(State {
        ring: Ring::new(cap),
        cursor: 0,
        exit: None,
        ended: None,
        stop_unconfirmed: false,
        background_left: false,
    }));
    let (tx, rx) = watch::channel(false);
    let mut stdin = child.stdin.take();
    // only with `input` is stdin written and closed; otherwise it stays open for job:write
    // (the tuple form took stdin unconditionally, closing it for every job)
    if let Some(inp) = input
        && let Some(mut w) = stdin.take()
    {
        let data = inp.as_bytes().to_vec();
        tokio::spawn(async move {
            let _ = w.write_all(&data).await;
            // dropping `w` closes stdin: the shell sees EOF after the script
        });
    }
    let out = child.stdout.take();
    let err = child.stderr.take();
    spawn_pipe_monitor(
        PipeMonitorArgs {
            state: state.clone(),
            tx,
            tree,
            out,
            err,
            od: super::outdecode::StreamDecoder::new(),
            ed: super::outdecode::StreamDecoder::new(),
            spill: spill.map(tokio::fs::File::from_std),
        },
        PipeMonitorMode::Running(Box::new(child)),
    );
    Ok(Job {
        pid,
        started: Instant::now(),
        state,
        exited: rx,
        input: Input::Pipe(AsyncMutex::new(stdin)),
        sandbox,
        lease,
    })
}

struct PipeMonitorArgs {
    state: Arc<Mutex<State>>,
    tx: watch::Sender<bool>,
    tree: super::trees::Tree,
    out: Option<tokio::process::ChildStdout>,
    err: Option<tokio::process::ChildStderr>,
    od: super::outdecode::StreamDecoder,
    ed: super::outdecode::StreamDecoder,
    spill: Option<tokio::fs::File>,
}

enum PipeMonitorMode {
    Running(Box<tokio::process::Child>),
    Lingering(Option<std::process::ExitStatus>),
}

async fn wait_tree_empty(tree: super::trees::Tree) {
    // the leader is gone, but what it started in the background may still be running in its
    // group: keep the group registered (ended with the host / mare) until the tree is empty
    while crate::platform::tree_alive(tree.pid()) {
        tokio::time::sleep(TREE_POLL_INTERVAL).await;
    }
    drop(tree);
}

fn spawn_pipe_monitor(args: PipeMonitorArgs, mode: PipeMonitorMode) {
    let PipeMonitorArgs { state: st, tx, tree, mut out, mut err, mut od, mut ed, mut spill } = args;
    tokio::spawn(async move {
        // held until the tree is gone: dropped earlier, the task went with the runtime and the
        // tree goes with it (`kill_on_drop` alone would end only the root)
        let (mut ob, mut eb) = (vec![0u8; 16384], vec![0u8; 16384]);
        let exit_status = match mode {
            PipeMonitorMode::Running(mut child) => {
                let mut status = None;
                // after the process exits, children it left behind may keep the pipes open: drain
                // what is buffered for a moment, then mark `background_left` and keep draining
                let mut grace: Option<tokio::time::Instant> = None;
                loop {
                    if out.is_none() && err.is_none() && status.is_some() {
                        break;
                    }
                    tokio::select! {
                        r = async { out.as_mut().unwrap().read(&mut ob).await }, if out.is_some() => {
                            let text = match r {
                                Ok(n) if n > 0 => od.push(&ob[..n]),
                                _ => {
                                    out = None;
                                    od.finish()
                                }
                            };
                            if !text.is_empty() {
                                st.lock().unwrap().ring.push(Stream::Out, text.as_bytes());
                                if let Some(f) = spill.as_mut() {
                                    let _ = f.write_all(text.as_bytes()).await;
                                }
                            }
                        },
                        r = async { err.as_mut().unwrap().read(&mut eb).await }, if err.is_some() => {
                            let text = match r {
                                Ok(n) if n > 0 => ed.push(&eb[..n]),
                                _ => {
                                    err = None;
                                    ed.finish()
                                }
                            };
                            if !text.is_empty() {
                                st.lock().unwrap().ring.push(Stream::Err, text.as_bytes());
                                if let Some(f) = spill.as_mut() {
                                    let _ = f.write_all(text.as_bytes()).await;
                                }
                            }
                        },
                        s = child.wait(), if status.is_none() => {
                            status = Some(s);
                            grace = Some(tokio::time::Instant::now() + EXIT_GRACE_PERIOD);
                        }
                        _ = async { tokio::time::sleep_until(grace.unwrap()).await }, if grace.is_some() => {
                            grace = None;
                            if out.is_some() || err.is_some() {
                                st.lock().unwrap().background_left = true;
                            } else {
                                break;
                            }
                        }
                    }
                }
                match status {
                    Some(s) => s.ok(),
                    None => child.wait().await.ok(),
                }
            }
            PipeMonitorMode::Lingering(leader_exit) => {
                while out.is_some() || err.is_some() {
                    tokio::select! {
                        r = async { out.as_mut().unwrap().read(&mut ob).await }, if out.is_some() => {
                            let text = match r {
                                Ok(n) if n > 0 => od.push(&ob[..n]),
                                _ => {
                                    out = None;
                                    od.finish()
                                }
                            };
                            if !text.is_empty() {
                                st.lock().unwrap().ring.push(Stream::Out, text.as_bytes());
                                if let Some(f) = spill.as_mut() {
                                    let _ = f.write_all(text.as_bytes()).await;
                                }
                            }
                        },
                        r = async { err.as_mut().unwrap().read(&mut eb).await }, if err.is_some() => {
                            let text = match r {
                                Ok(n) if n > 0 => ed.push(&eb[..n]),
                                _ => {
                                    err = None;
                                    ed.finish()
                                }
                            };
                            if !text.is_empty() {
                                st.lock().unwrap().ring.push(Stream::Err, text.as_bytes());
                                if let Some(f) = spill.as_mut() {
                                    let _ = f.write_all(text.as_bytes()).await;
                                }
                            }
                        },
                    }
                }
                leader_exit
            }
        };
        if let Some(f) = spill.as_mut() {
            let _ = f.flush().await;
        }
        let exit = match exit_status {
            Some(s) => {
                let signal = <platform::Process as ProcessBackend>::exit_signal(&s);
                Exit { code: s.code(), signal }
            }
            None => Exit::default(),
        };
        {
            let mut g = st.lock().unwrap();
            g.exit = Some(exit);
            g.ended = Some(Instant::now());
        }
        let _ = tx.send(true);
        wait_tree_empty(tree).await;
    });
}

/// Adopt a lingering process tree whose shell exited while descendants still hold output pipes
/// open (`mare.proc.run` `background_left`), returning a tracked `Job` handle.
#[allow(clippy::too_many_arguments)]
pub(crate) fn from_lingering(
    pid: Option<u32>,
    tree: super::trees::Tree,
    leader_exit: Option<std::process::ExitStatus>,
    out: Option<tokio::process::ChildStdout>,
    err: Option<tokio::process::ChildStderr>,
    od: super::outdecode::StreamDecoder,
    ed: super::outdecode::StreamDecoder,
    spill: Option<tokio::fs::File>,
    sandbox: Option<super::ossandbox::Prepared>,
) -> Job {
    let state = Arc::new(Mutex::new(State {
        ring: Ring::new(DEFAULT_CAP),
        cursor: 0,
        exit: None,
        ended: None,
        stop_unconfirmed: false,
        background_left: true,
    }));
    let (tx, rx) = watch::channel(false);
    spawn_pipe_monitor(
        PipeMonitorArgs { state: state.clone(), tx, tree, out, err, od, ed, spill },
        PipeMonitorMode::Lingering(leader_exit),
    );
    Job {
        pid,
        started: Instant::now(),
        state,
        exited: rx,
        input: Input::Pipe(AsyncMutex::new(None)),
        sandbox,
        lease: None,
    }
}

/// Open (truncate) the spill file only after the command's own sandbox was prepared: a refused
/// sandbox must leave the named file untouched, and its deny_read covers the spill too.
fn open_spill(
    lua: &Lua,
    path: Option<&str>,
    prefix: Option<&str>,
    sandbox: Option<&super::ossandbox::Prepared>,
) -> mlua::Result<Option<std::fs::File>> {
    let Some(p) = path else { return Ok(None) };
    let p = super::proc::checked_spill_path(lua, p, prefix, sandbox.map(|sb| &sb.policy))?;
    if let Some(d) = p.parent() {
        mare_shared::private::create_dir_all(d).map_err(rt_err)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.create(true).write(true).truncate(true);
    mare_shared::private::open(&mut o, &p).map_err(rt_err).map(Some)
}

#[allow(clippy::too_many_arguments)]
fn start_pty(
    lua: &Lua,
    o: &super::proc::SpawnOpts,
    default_cwd: &str,
    input: Option<mlua::LuaString>,
    cap: usize,
    spill: Option<&str>,
    spill_prefix: Option<&str>,
    cols: u16,
    rows: u16,
    lease: super::pty::lease::Lease,
) -> mlua::Result<Job> {
    let cwd_str = o.cwd.clone().unwrap_or_else(|| default_cwd.to_string());
    let prepared = super::ossandbox::Prepared::new(lua, &o.sandbox, std::path::Path::new(&cwd_str))?;
    let spill = open_spill(lua, spill, spill_prefix, prepared.as_ref())?;
    let pty = Arc::new(super::pty::Pty::spawn_for_job(
        &o.cmd,
        &o.args,
        &cwd_str,
        &o.env,
        o.env_policy.clone(),
        o.clear_env,
        cols,
        rows,
        prepared,
        Some(super::trees::Owner::of(lua)),
        lease,
    )?);
    pty.register_lease(lua);
    if let Some(inp) = input {
        pty.write(&inp.as_bytes())?;
    }
    let pid = pty.pid();
    let state = Arc::new(Mutex::new(State {
        ring: Ring::new(cap),
        cursor: 0,
        exit: None,
        ended: None,
        stop_unconfirmed: false,
        background_left: false,
    }));
    let (tx, rx) = watch::channel(false);
    let st = state.clone();
    let pty_task = pty.clone();
    tokio::spawn(async move {
        let mut spill = spill.map(tokio::fs::File::from_std);
        let mut od = super::outdecode::StreamDecoder::new();
        while let Some(chunk) = pty_task.read_chunk().await {
            let text = od.push(&chunk);
            if !text.is_empty() {
                st.lock().unwrap().ring.push(Stream::Out, text.as_bytes());
                if let Some(f) = spill.as_mut() {
                    let _ = f.write_all(text.as_bytes()).await;
                }
            }
        }
        let tail = od.finish();
        if !tail.is_empty() {
            st.lock().unwrap().ring.push(Stream::Out, tail.as_bytes());
            if let Some(f) = spill.as_mut() {
                let _ = f.write_all(tail.as_bytes()).await;
            }
        }
        if let Some(f) = spill.as_mut() {
            let _ = f.flush().await;
        }
        let exit_code = pty_task.wait_exit_after_eof(|| false).await;
        {
            let mut g = st.lock().unwrap();
            g.exit = Some(Exit { code: exit_code, signal: None });
            g.ended = Some(Instant::now());
        }
        let _ = tx.send(true);
    });
    Ok(Job { pid, started: Instant::now(), state, exited: rx, input: Input::Pty(pty), sandbox: None, lease: None })
}

pub fn install(lua: &Lua, mare: &Table, cwd: String) -> mlua::Result<()> {
    // groups a killed predecessor left running (macOS; once per process, off the startup path)
    static SWEEP: Once = Once::new();
    SWEEP.call_once(|| {
        let _ = std::thread::Builder::new().name("mare-pgroup-sweep".into()).spawn(|| {
            let ended = super::pgroups::sweep_stale();
            if !ended.is_empty() {
                tracing::info!("ended {} process group(s) left by a mare that was killed: {ended:?}", ended.len());
            }
        });
    });
    let proc: Table = mare.get("proc")?;
    proc.set("start", lua.create_function(move |lua, opts: Table| start(lua, &opts, &cwd))?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_head_and_tail() {
        let mut r = Ring::new(10); // 5 head + 5 tail
        r.push(Stream::Out, b"hello ");
        r.push(Stream::Err, b"world!!");
        assert_eq!(r.total, 13);
        // head, then the gap marked inline, then the newest bytes
        let (d, next, dropped) = r.read(0, 100, None);
        let d = String::from_utf8(d).unwrap();
        assert!(d.starts_with("hello") && d.ends_with("rld!!") && d.contains("3 bytes omitted"), "{d}");
        assert_eq!((next, dropped), (13, 3));
        // a cursor inside the dropped middle: counted, not marked
        let (d, next, dropped) = r.read(6, 100, None);
        assert_eq!((d.as_slice(), next, dropped), (&b"rld!!"[..], 13, 2));
        let (d, _, _) = r.read(8, 100, Some(Stream::Err));
        assert_eq!(d, b"rld!!");
    }

    #[cfg(unix)]
    fn gone(pid: i32) -> bool {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        // killed but not yet reaped by its new parent: gone all the same
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|s| s.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')))
    }

    /// The runtime going away while a job runs (mare exiting, a test's runtime dropped) ends the
    /// job's whole tree, not only its root — even while the job itself is still held (Lua keeps
    /// it until garbage collection). On Windows a survivor holding the output pipes left the
    /// pipe reader's blocking read pending, and the runtime's shutdown waited for it forever.
    #[test]
    fn dropping_the_runtime_ends_the_whole_tree() {
        let lua = Lua::new();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let opts = lua.create_table().unwrap();
        // the root starts a grandchild that inherits the output pipes
        #[cfg(unix)]
        let (cmd, args) = ("sh", vec!["-c", "sleep 120 & echo $!; wait"]);
        #[cfg(windows)]
        let (cmd, args) = ("cmd", vec!["/c", "ping -n 120 127.0.0.1 >NUL"]);
        opts.set("cmd", cmd).unwrap();
        opts.set("args", args).unwrap();
        let job = {
            let _rt = rt.enter();
            start(&lua, &opts, &cwd).unwrap()
        };
        let pid = job.pid.unwrap();
        #[cfg(unix)]
        let grandchild: i32 = rt.block_on(async {
            for _ in 0..500 {
                let text = {
                    let st = job.state.lock().unwrap();
                    String::from_utf8_lossy(&st.ring.read(0, 64, None).0).into_owned()
                };
                if let Some(line) = text.lines().next().filter(|_| text.contains('\n')) {
                    return line.trim().parse().unwrap();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("the job never printed its grandchild's pid");
        });
        #[cfg(windows)]
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(500)).await });
        // dropped on another thread: a shutdown that hangs fails the test instead of hanging it
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(rt);
            let _ = tx.send(());
        });
        assert!(rx.recv_timeout(Duration::from_secs(30)).is_ok(), "the runtime's shutdown hung");
        let deadline = Instant::now() + Duration::from_secs(10);
        #[cfg(unix)]
        let ended = || gone(grandchild);
        #[cfg(windows)]
        let ended = || !crate::platform::tree_alive(Some(pid));
        while !ended() {
            assert!(Instant::now() < deadline, "the job's tree outlived the runtime (root {pid})");
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(job);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_prefers_cancel_when_timeout_and_cancel_are_both_ready() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let lua = Lua::new();
                let (_tx, rx) = watch::channel(false);
                let state = Arc::new(Mutex::new(State {
                    ring: Ring::new(1024),
                    cursor: 0,
                    exit: None,
                    ended: None,
                    stop_unconfirmed: false,
                    background_left: false,
                }));
                let job = Job {
                    pid: None,
                    started: Instant::now(),
                    state,
                    exited: rx,
                    input: Input::Pipe(AsyncMutex::new(None)),
                    sandbox: None,
                    lease: None,
                };
                let job_ud = lua.create_userdata(job).unwrap();
                lua.globals().set("job", job_ud).unwrap();
                let mare = lua.create_table().unwrap();
                super::super::task::install(&lua, &mare).unwrap();
                lua.globals().set("mare", mare).unwrap();
                let wait_fn: mlua::Function =
                    lua.load("return function(tok) return job:wait({ timeout_ms = 50, cancel = tok }) end").eval().unwrap();
                for i in 0..30 {
                    let tok = tokio_util::sync::CancellationToken::new();
                    let ud = lua.create_userdata(super::super::task::Token(tok.clone())).unwrap();
                    let f = wait_fn.clone();
                    let handle =
                        tokio::task::spawn_local(async move { f.call_async::<Table>(ud).await.unwrap() });
                    tokio::task::yield_now().await;
                    tok.cancel();
                    tokio::time::advance(Duration::from_millis(100)).await;
                    let st = handle.await.unwrap();
                    let cancelled: bool = st.get("cancelled").unwrap();
                    let timed_out: bool = st.get("timed_out").unwrap();
                    assert!(
                        cancelled && !timed_out,
                        "iteration {i}: expected cancelled=true, timed_out=false; got cancelled={cancelled}, timed_out={timed_out}"
                    );
                }
            })
            .await;
    }

    #[test]
    fn repro_issue_203_ring_buffer_cuts_inside_multibyte_utf8_character() {
        // Capacity 20 bytes: head_cap = 10, cap = 20.
        // Push 10 ASCII bytes (fills head) + 10 3-byte UTF-8 chars ("中" = E4 B8 AD, 30 bytes > cap 20).
        // Ring drains 10 bytes from front of tail (10 % 3 != 0), cutting inside a valid UTF-8 character.
        let mut ring = Ring::new(20);
        ring.push(Stream::Out, b"0123456789");
        let cjk = "中".repeat(10);
        ring.push(Stream::Out, cjk.as_bytes());
        let (bytes, _next, dropped) = ring.read(10, 64, None);
        assert!(dropped > 0, "expected tail overflow drop");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains('\u{FFFD}'),
            "ring tail cut inside valid UTF-8 character, producing U+FFFD: {text:?} (raw={bytes:?})"
        );
    }

    #[test]
    fn ring_preserves_utf8_on_head_gap_and_max_read_and_keeps_genuinely_invalid_bytes() {
        // 1. Head->gap->tail cut inside multi-byte UTF-8: head_cap = 10 cuts 4th "中" (bytes 9..12).
        let mut ring = Ring::new(20);
        let cjk = "中".repeat(12); // 36 bytes
        ring.push(Stream::Out, cjk.as_bytes());
        let (bytes, next, dropped) = ring.read(0, 128, None);
        assert_eq!(next, 36);
        assert!(dropped > 0);
        let text = std::str::from_utf8(&bytes).expect("head + marker + tail must be valid UTF-8");
        assert!(text.starts_with("中中中"), "head should keep complete chars: {text:?}");
        assert!(text.ends_with("中中中"), "tail should keep complete chars: {text:?}");
        assert!(text.contains("bytes omitted"), "gap marker missing: {text:?}");

        // 2. Reading with `max` cutting inside a valid UTF-8 char aligns to whole chars and preserves offsets.
        let mut ring2 = Ring::new(64);
        ring2.push(Stream::Out, "a中b中c".as_bytes()); // 1 + 3 + 1 + 3 + 1 = 9 bytes
        let (part1, off1, _) = ring2.read(0, 3, None); // byte 3 is mid-"中"
        assert_eq!(std::str::from_utf8(&part1).unwrap(), "a");
        assert_eq!(off1, 1);
        let (part2, off2, _) = ring2.read(off1, 4, None); // "中b" = 4 bytes
        assert_eq!(std::str::from_utf8(&part2).unwrap(), "中b");
        assert_eq!(off2, 5);
        let (part3, off3, _) = ring2.read(off2, 16, None);
        assert_eq!(std::str::from_utf8(&part3).unwrap(), "中c");
        assert_eq!(off3, 9);

        // 3. Genuinely invalid UTF-8 bytes after an eviction are NOT swallowed as valid continuations.
        let mut ring3 = Ring::new(10); // head_cap = 5, cap = 5
        ring3.push(Stream::Out, b"01234");
        // Push 6 bytes into cap 5: byte 0 is 0xFF (evicted), followed by invalid lone 0x80 + "abcd" (5 bytes).
        ring3.push(Stream::Out, b"\xff\x80abcd");
        let (raw, next3, dropped3) = ring3.read(5, 64, None);
        assert_eq!(dropped3, 1);
        assert_eq!(next3, 11);
        assert_eq!(raw, b"\x80abcd", "genuinely invalid 0x80 after 0xFF must be preserved");
        assert_eq!(ring3.tail(5), b"\x80abcd");

        // 4. Multi-chunk read backoff must stop immediately without treating buffered bytes as a gap.
        let mut ring4 = Ring::new(10); // head_cap = 5 ("a中b"), tail = "cdef"
        ring4.push(Stream::Out, "a中bcdef".as_bytes());
        let (p1, n1, d1) = ring4.read(0, 3, None);
        assert_eq!(std::str::from_utf8(&p1).unwrap(), "a");
        assert_eq!((n1, d1), (1, 0), "backed-off read must stop at first unconsumed byte with 0 dropped");
        let (p2, n2, d2) = ring4.read(n1, 4, None);
        assert_eq!(std::str::from_utf8(&p2).unwrap(), "中b");
        assert_eq!((n2, d2), (5, 0));
        let (p3, n3, d3) = ring4.read(n2, 16, None);
        assert_eq!(std::str::from_utf8(&p3).unwrap(), "cdef");
        assert_eq!((n3, d3), (9, 0));

        // 5. Cross-chunk multi-byte UTF-8 character split across head/tail boundary (no eviction).
        let mut ring5 = Ring::new(6); // head_cap = 3 ("ab\xe4"), tail = "\xb8\xadc" (6 bytes total)
        ring5.push(Stream::Out, "ab中c".as_bytes());
        let (c1, cn1, cd1) = ring5.read(0, 3, None);
        assert_eq!(std::str::from_utf8(&c1).unwrap(), "ab");
        assert_eq!((cn1, cd1), (2, 0));
        let (c2, cn2, cd2) = ring5.read(0, 4, None);
        assert_eq!(std::str::from_utf8(&c2).unwrap(), "ab");
        assert_eq!((cn2, cd2), (2, 0));
        let (c3, cn3, cd3) = ring5.read(2, 4, None);
        assert_eq!(std::str::from_utf8(&c3).unwrap(), "中c");
        assert_eq!((cn3, cd3), (6, 0));

        // 5b. Cross-chunk split at head/tail followed immediately by eviction of the 1st continuation byte.
        let mut ring5b = Ring::new(4); // head_cap = 2 ("a\xe4"), cap = 2, push "a中b" (5 bytes -> evicts "\xb8" + "\xad")
        ring5b.push(Stream::Out, "a中b".as_bytes());
        let (ev_out, ev_next, ev_drop) = ring5b.read(0, 32, None);
        let ev_text = std::str::from_utf8(&ev_out).expect("head-split eviction must produce valid UTF-8");
        assert!(ev_text.starts_with('a') && ev_text.ends_with('b'), "{ev_text:?}");
        assert_eq!((ev_next, ev_drop), (5, 3));

        // 6. Budget smaller than one multi-byte codepoint returns empty valid UTF-8 with 0 consumed/dropped.
        let mut ring6 = Ring::new(16);
        ring6.push(Stream::Out, "中a".as_bytes());
        let (s1, sn1, sd1) = ring6.read(0, 2, None);
        assert_eq!(s1, b"");
        assert_eq!((sn1, sd1), (0, 0));
        // tail(2) on "中" must align past continuation bytes (empty valid UTF-8).
        let mut ring7 = Ring::new(16);
        ring7.push(Stream::Out, "中".as_bytes());
        assert_eq!(ring7.tail(2), b"", "tail(2) on 3-byte char must not start on continuation byte");
        let mut ring8 = Ring::new(16);
        ring8.push(Stream::Out, b"\xff\x80a");
        assert_eq!(ring8.tail(2), b"\x80a", "genuinely invalid bytes preserved by tail");
    }
}
