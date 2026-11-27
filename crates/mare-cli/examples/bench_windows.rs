//! Native Windows counterpart of scripts/bench.py. Run through `just bench`.

#[cfg(not(windows))]
fn main() {
    eprintln!("bench_windows is only available on Windows; use just bench");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    if let Err(err) = bench::run() {
        eprintln!("benchmark incomplete: {err:#}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod bench {
    use std::ffi::OsString;
    use std::fmt::Write as _;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::Path;
    use std::sync::OnceLock;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, anyhow, bail, ensure};
    use mare_binding::platform::{Terminal, TerminalCommand, TerminalWriter};
    use regex::bytes::Regex;
    use serde_json::json;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    const INPUT: &[u8] = "╰".as_bytes();
    const START: &str = "MARE_BENCH_STREAM_STARTED";
    const END: &str = "MARE_BENCH_STREAM_FINISHED";
    const STREAM_END_TIMEOUT: Duration = Duration::from_secs(180);

    /// The reader must keep draining when ConPTY closes; stopping the child before joining it
    /// avoids the full-pipe deadlock described in the Windows PTY tests.
    struct Pty {
        term: Option<Terminal>,
        writer: Option<TerminalWriter>,
        reader: Option<JoinHandle<()>>,
        rx: Receiver<Vec<u8>>,
        output: Vec<u8>,
        query_tail: Vec<u8>,
    }

    impl Pty {
        fn spawn(bin: &Path, home: &Path, cwd: &Path, args: &[String], extra: &[(&str, &Path)]) -> Result<Self> {
            let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
                .filter(|(name, _)| {
                    let key = name.to_string_lossy().to_ascii_uppercase();
                    !key.starts_with("MARE_")
                        && !key.ends_with("_API_KEY")
                        && !matches!(key.as_str(), "HOME" | "USERPROFILE" | "TERM")
                })
                .collect();
            env.extend([
                ("MARE_HOME".into(), home.into()),
                ("HOME".into(), home.into()),
                ("USERPROFILE".into(), home.into()),
                ("TERM".into(), "xterm-256color".into()),
                ("MARE_UPDATE_CHECK".into(), "false".into()),
            ]);
            env.extend(extra.iter().map(|(key, value)| (OsString::from(key), value.as_os_str().into())));
            let bin = bin.to_str().context("benchmark executable path is not Unicode")?;
            let cwd = cwd.to_str().context("benchmark working directory is not Unicode")?;
            let (term, mut reader, writer) =
                Terminal::spawn(TerminalCommand { prog: bin, args, cwd, env, cols: 100, rows: 30 })
                    .context("starting mare in ConPTY")?;
            let (tx, rx) = mpsc::channel();
            let reader = thread::spawn(move || {
                let mut buf = [0; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) if tx.send(buf[..n].to_vec()).is_err() => break,
                        Ok(_) => {}
                    }
                }
            });
            Ok(Self {
                term: Some(term),
                writer: Some(writer),
                reader: Some(reader),
                rx,
                output: vec![],
                query_tail: vec![],
            })
        }

        fn pid(&self) -> u32 {
            self.term.as_ref().expect("terminal is open").pid()
        }

        fn pump(&mut self, timeout: Duration) -> Result<()> {
            match self.rx.recv_timeout(timeout) {
                Ok(chunk) => {
                    let mut q = std::mem::take(&mut self.query_tail);
                    q.extend_from_slice(&chunk);
                    let count = q.windows(4).filter(|w| *w == b"\x1b[6n").count();
                    self.query_tail = q[q.len().saturating_sub(3)..].to_vec();
                    if let Some(writer) = &mut self.writer {
                        for _ in 0..count {
                            writer.write_all(b"\x1b[1;1R").context("answering terminal cursor query")?;
                        }
                    }
                    self.output.extend_from_slice(&chunk);
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
            }
            Ok(())
        }

        fn wait_for(&mut self, needle: &[u8], timeout: Duration, plain: bool) -> Result<()> {
            static ANSI: OnceLock<Regex> = OnceLock::new();
            let ansi = ANSI.get_or_init(|| {
                Regex::new(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[@-Z\\-_])").unwrap()
            });
            let deadline = Instant::now() + timeout;
            loop {
                let text = if plain { ansi.replace_all(&self.output, &b""[..]) } else { (&self.output[..]).into() };
                if text.windows(needle.len()).any(|w| w == needle) {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    bail!("timed out waiting for {:?}", String::from_utf8_lossy(needle));
                }
                if let Some(status) = self.term.as_mut().expect("terminal is open").try_wait()? {
                    // The reader may still have the child's final output in its pipe.
                    self.pump(Duration::from_millis(50))?;
                    let text = if plain { ansi.replace_all(&self.output, &b""[..]) } else { (&self.output[..]).into() };
                    if text.windows(needle.len()).any(|w| w == needle) {
                        return Ok(());
                    }
                    bail!("mare exited with {status} before {:?}", String::from_utf8_lossy(needle));
                }
                self.pump(Duration::from_millis(10))?;
            }
        }

        fn drain(&mut self, duration: Duration) -> Result<()> {
            let end = Instant::now() + duration;
            while let Some(left) = end.checked_duration_since(Instant::now()) {
                self.pump(left.min(Duration::from_millis(50)))?;
            }
            Ok(())
        }

        fn send(&mut self, bytes: &[u8]) -> Result<()> {
            let writer = self.writer.as_mut().context("terminal input is closed")?;
            writer.write_all(bytes)?;
            writer.flush()?;
            Ok(())
        }

        fn send_line(&mut self, text: &[u8]) -> Result<()> {
            if !text.is_empty() {
                self.send(text)?;
                // Windows enables unbracketed PasteBurst (120ms enter suppress window); pause past it
                // so Enter submits instead of inserting a literal newline.
                // sleep-ok: separates synthetic line input from Enter past the PasteBurst window
                std::thread::sleep(Duration::from_millis(160));
            }
            self.send(b"\r")
        }

        fn stop(&mut self) -> Result<()> {
            let Some(mut term) = self.term.take() else { return Ok(()) };
            term.kill().context("stopping benchmark process")?;
            let end = Instant::now() + Duration::from_secs(10);
            while term.try_wait()?.is_none() && Instant::now() < end {
                self.pump(Duration::from_millis(20))?;
            }
            ensure!(term.try_wait()?.is_some(), "benchmark process did not exit within 10 seconds");
            self.writer.take();
            drop(term);
            if let Some(reader) = self.reader.take() {
                reader.join().map_err(|_| anyhow!("ConPTY reader panicked"))?;
            }
            Ok(())
        }
    }

    impl Drop for Pty {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }

    struct Process(HANDLE);

    impl Process {
        fn open(pid: u32) -> Result<Self> {
            // SAFETY: querying a process started by this benchmark; the returned handle is owned.
            let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error()).context("opening mare process for counters");
            }
            Ok(Self(handle))
        }

        fn rss_mb(&self) -> Result<f64> {
            let mut counters = PROCESS_MEMORY_COUNTERS::default();
            let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            counters.cb = size;
            // SAFETY: the handle is live and counters points to the stated writable structure.
            if unsafe { GetProcessMemoryInfo(self.0, &mut counters, size) } == 0 {
                return Err(std::io::Error::last_os_error()).context("reading process working set");
            }
            Ok(counters.WorkingSetSize as f64 / 1024.0 / 1024.0)
        }

        fn cpu_ms(&self) -> Result<f64> {
            let (mut created, mut exited, mut kernel, mut user) =
                (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            // SAFETY: the process handle is live and all four output pointers are valid.
            if unsafe { GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
                return Err(std::io::Error::last_os_error()).context("reading process CPU time");
            }
            let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
            Ok((ticks(kernel) + ticks(user)) as f64 / 10_000.0)
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: this handle was returned by OpenProcess and is closed exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn make_home() -> Result<tempfile::TempDir> {
        let home = tempfile::Builder::new().prefix("mare-bench-home-").tempdir()?;
        fs::create_dir_all(home.path().join("config"))?;
        fs::write(
            home.path().join("config/init.lua"),
            "mare.setup { model = \"faux/echo\", update_check = false }\n\
             mare.plugin(\"llm\", { models_dev = false })\n\
             mare.provider(\"ollama\", { list_models = false })\n",
        )?;
        Ok(home)
    }

    fn plugin_max(path: &Path) -> Result<f64> {
        let data = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut lines = data.lines();
        ensure!(
            lines.next().unwrap_or_default().split_whitespace().collect::<Vec<_>>() == ["clock", "self", "step"],
            "unrecognised startuptime header"
        );
        let mut max: Option<f64> = None;
        for line in lines {
            let mut cols = line.split_whitespace();
            let clock: f64 = cols.next().context("missing startup clock")?.parse()?;
            let elapsed: f64 = cols.next().context("missing startup duration")?.parse()?;
            let label = cols.collect::<Vec<_>>().join(" ");
            ensure!(clock.is_finite() && elapsed.is_finite() && !label.is_empty(), "invalid startuptime row: {line}");
            let base_label = label.split_once(" [").map_or(label.as_str(), |(head, _)| head);
            if base_label.starts_with("plugin ")
                && [" (builtin)", " (package)", " (user)", " (project)"].iter().any(|s| base_label.ends_with(s))
            {
                max = Some(max.map_or(elapsed, |prev| prev.max(elapsed)));
            }
        }
        max.context("no plugin load rows in startuptime file")
    }

    fn wait_for_plugin_max(pty: &mut Pty, path: &Path) -> Result<f64> {
        // The first input frame is shown while plugins are still starting. On slower machines,
        // the profile written after host startup can appear well after that frame.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match plugin_max(path) {
                Ok(max) => return Ok(max),
                Err(err) if Instant::now() >= deadline => return Err(err).context("waiting for startup profile"),
                Err(_) => {}
            }
            if let Some(status) = pty.term.as_mut().expect("terminal is open").try_wait()? {
                bail!("mare exited with {status} before writing its startup profile");
            }
            pty.pump(Duration::from_millis(20))?;
        }
    }

    fn startup(bin: &Path, home: &Path, work: &Path, runs: usize) -> Result<(f64, f64, usize)> {
        let mut times = Vec::new();
        let mut max = 0.0_f64;
        for i in 0..runs {
            let file = home.join(format!("startuptime-{i}.txt"));
            let args = vec!["--startuptime".into(), file.to_string_lossy().into_owned()];
            let start = Instant::now();
            let mut pty = Pty::spawn(bin, home, work, &args, &[])?;
            match pty.wait_for(INPUT, Duration::from_secs(10), false) {
                Ok(()) => {
                    times.push(start.elapsed().as_secs_f64() * 1000.0);
                    pty.drain(Duration::from_millis(150))?;
                    max = max.max(wait_for_plugin_max(&mut pty, &file)?);
                    pty.stop()?;
                }
                Err(err) => {
                    eprintln!("startup sample {}/{}: {err:#}", i + 1, runs);
                    pty.stop()?;
                }
            }
        }
        ensure!(!times.is_empty(), "no startup sample reached the input frame");
        times.sort_by(f64::total_cmp);
        let middle = times.len() / 2;
        let median = if times.len() % 2 == 0 { (times[middle - 1] + times[middle]) / 2.0 } else { times[middle] };
        Ok((median, max, times.len()))
    }

    fn idle(bin: &Path, home: &Path, work: &Path) -> Result<f64> {
        let mut pty = Pty::spawn(bin, home, work, &[], &[])?;
        pty.wait_for(INPUT, Duration::from_secs(10), false).context("idle memory: input frame")?;
        pty.drain(Duration::from_secs(5))?;
        let memory = Process::open(pty.pid())?.rss_mb()?;
        pty.stop()?;
        Ok(memory)
    }

    fn idle_cpu(bin: &Path, home: &Path, work: &Path) -> Result<f64> {
        let mut pty = Pty::spawn(bin, home, work, &[], &[])?;
        pty.wait_for(INPUT, Duration::from_secs(10), false).context("idle CPU: input frame")?;
        pty.drain(Duration::from_secs(2))?;
        let proc = Process::open(pty.pid())?;
        let before = proc.cpu_ms()?;
        pty.drain(Duration::from_secs(10))?;
        let cpu = proc.cpu_ms()? - before;
        ensure!(cpu >= 0.0, "idle CPU counter moved backwards");
        pty.stop()?;
        Ok(cpu)
    }

    fn write_session(home: &Path, cwd: &Path) -> Result<()> {
        let dir = home.join("state/sessions").join(mare_store::cwd_slug(cwd));
        fs::create_dir_all(&dir)?;
        let mut file = fs::File::create(dir.join("2026-01-01T00-00-00_bench.jsonl"))?;
        writeln!(
            file,
            "{}",
            json!({"type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "bench", "cwd": cwd, "created_at_ms": 1})
        )?;
        let mut parent: Option<String> = None;
        for i in 0..100 {
            let user = json!({"type": "message", "entry_id": format!("u{i}"), "parent_entry_id": parent,
                "message": {"role": "user", "content": [{"type": "text", "text": format!("question {i}: explain step {i}")}]}});
            let body = format!(
                "## Step {i}\n\nHere is **some** explanation with `inline code` and a list:\n\n- one\n- two\n\n```rust\nfn main() {{\n    println!(\"hello\");\n}}\n```\n"
            );
            let assistant = json!({"type": "message", "entry_id": format!("a{i}"), "parent_entry_id": format!("u{i}"),
                "message": {"role": "assistant", "content": [{"type": "text", "text": body}],
                    "usage": {"input": 1000 + i, "output": 200}}});
            writeln!(file, "{user}\n{assistant}")?;
            parent = Some(format!("a{i}"));
        }
        Ok(())
    }

    fn replay(bin: &Path, home: &Path) -> Result<(f64, f64)> {
        let work = tempfile::Builder::new().prefix("mare-bench-cwd-").tempdir()?;
        let cwd = dunce::canonicalize(work.path())?;
        write_session(home, &cwd)?;
        let start = Instant::now();
        let mut pty = Pty::spawn(bin, home, &cwd, &["-c".into()], &[])?;
        pty.wait_for(INPUT, Duration::from_secs(20), false).context("session replay: input frame")?;
        let resume = start.elapsed().as_secs_f64() * 1000.0;
        pty.drain(Duration::from_secs(3))?;
        let memory = Process::open(pty.pid())?.rss_mb()?;
        pty.stop()?;
        Ok((memory, resume))
    }

    fn frame_values(path: &Path) -> Result<Vec<f64>> {
        fs::read_to_string(path)
            .with_context(|| format!("reading frame log {}", path.display()))?
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(|v| v.parse::<f64>().map(|us| us / 1000.0).map_err(Into::into))
            .collect()
    }

    fn frames(bin: &Path, home: &Path, work: &Path) -> Result<(f64, usize, usize)> {
        let mut text = format!("{START}\n\n");
        for i in 0..120 {
            if i > 0 {
                text.push_str("\n\n");
            }
            write!(
                text,
                "### Section {i}\n\nSome streaming text with **bold**, `code`, and a table:\n\n| a | b |\n|---|---|\n| {i} | {} |\n\n```python\nfor x in range(10):\n    print(x)\n```",
                i * 2
            )?;
        }
        write!(text, "\n\n{END}")?;
        let script = home.join("stream.json");
        let log = home.join("frames.log");
        fs::write(&script, serde_json::to_vec(&json!([{"text": text, "delay_ms": 3}]))?)?;
        let debug = Path::new("debug");
        let extra = [("MARE_FAUX_SCRIPT", script.as_path()), ("MARE_FRAME_LOG", log.as_path()), ("MARE_LOG", debug)];
        let mut pty = Pty::spawn(bin, home, work, &[], &extra)?;
        pty.wait_for(INPUT, Duration::from_secs(10), false).context("stream: input frame")?;
        pty.send_line(b"go")?;
        pty.wait_for(START.as_bytes(), Duration::from_secs(10), true).context("stream: start marker")?;
        let initial = frame_values(&log)?.len();
        // The same faux workload sleeps 3 ms per eight bytes; Windows' coarse timer can round
        // those sleeps to about 16 ms each, so give it time to finish without changing the work.
        pty.wait_for(END.as_bytes(), STREAM_END_TIMEOUT, true).context("stream: end marker")?;
        pty.drain(Duration::from_millis(250))?;
        pty.stop()?;
        let all = frame_values(&log)?;
        ensure!(all.len() >= initial, "frame log shrank during stream");
        let mut values = all[initial..].to_vec();
        ensure!(!values.is_empty() && values.iter().all(|v| v.is_finite() && *v >= 0.0), "invalid frame log");
        values.sort_by(f64::total_cmp);
        let p99 = values[((values.len() as f64 * 0.99) as usize).min(values.len() - 1)];
        let slow = fs::read_to_string(home.join("state/log/mare.log")).unwrap_or_default();
        let pattern = Regex::new(r"slow callback: \[[^\]]+\] \S+ ran \d+\.\dms \(budget 2ms\)")?;
        let callbacks = slow.lines().filter(|line| pattern.is_match(line.as_bytes())).count();
        Ok((p99, callbacks, values.len()))
    }

    pub fn run() -> Result<()> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let bin = args
            .iter()
            .find(|arg| !arg.starts_with("--"))
            .context("usage: bench_windows MARE.EXE [--check] [--keep]")?;
        let bin = dunce::canonicalize(bin).context("benchmark executable not found")?;
        let check = args.iter().any(|arg| arg == "--check");
        let keep = args.iter().any(|arg| arg == "--keep");
        let runs: usize =
            std::env::var("BENCH_RUNS").unwrap_or_else(|_| "10".into()).parse().context("invalid BENCH_RUNS")?;
        ensure!(runs > 0, "BENCH_RUNS must be positive");
        let home = make_home()?;
        let work = tempfile::Builder::new().prefix("mare-bench-work-").tempdir()?;
        let result = measure(&bin, home.path(), work.path(), runs, check);
        if keep {
            println!("benchmark artifacts: {}", home.keep().display());
        }
        result
    }

    fn measure(bin: &Path, home: &Path, work: &Path, runs: usize, check: bool) -> Result<()> {
        let (startup, plugin, samples) = startup(bin, home, work, runs)?;
        let idle = idle(bin, home, work)?;
        let (replay, resume) = replay(bin, home)?;
        let (p99, callbacks, frame_count) = frames(bin, home, work)?;
        let cpu = idle_cpu(bin, home, work)?;
        let rows = [
            (
                format!("startup → input (median of {samples}/{runs})"),
                format!("{startup:.1} ms"),
                "≤ 30 ms",
                startup <= 30.0,
            ),
            ("idle RSS (5s)".into(), format!("{idle:.1} MB"), "≤ 50 MB", idle <= 50.0),
            (
                "RSS after resuming 100 turns".into(),
                format!("{replay:.1} MB (input box after {resume:.0} ms)"),
                "≤ 120 MB",
                replay <= 120.0,
            ),
            (
                format!("frame time p99 (streaming, {frame_count} samples)"),
                format!("{p99:.2} ms"),
                "≤ 16 ms; ≥ 100 samples; completed",
                frame_count >= 100 && p99 <= 16.0,
            ),
            ("slow Lua callbacks (> 2ms)".into(), callbacks.to_string(), "logged", true),
            ("idle CPU (10s, after 2s)".into(), format!("{cpu:.0} ms"), "≤ 100 ms", cpu <= 100.0),
            ("slowest plugin load".into(), format!("{plugin:.2} ms"), "≤ 5 ms", plugin <= 5.0),
        ];
        let width = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
        for (name, value, reference, ok) in rows {
            println!("{} {name:<width$}  {value:<28} reference: {reference}", if ok { "within" } else { "review" });
        }
        println!("Windows native: RSS = process WorkingSetSize; CPU = process user + kernel time.");
        println!("Performance values are references, not pass/fail requirements.");
        let complete = samples == runs
            && [startup, plugin, idle, replay, resume, p99, cpu].iter().all(|v| v.is_finite())
            && plugin > 0.0
            && frame_count >= 100;
        if check {
            ensure!(complete, "missing startup samples or invalid/incomplete measurements");
        }
        Ok(())
    }
}
