//! Shared PTY test driver (`MareCmd` + `PtySession`), extracted from `suite/tui_pty.rs`.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mare_binding::platform::{Terminal, TerminalCommand};
use serde_json::Value;

use super::WAIT_SECS;

const POLL_INTERVAL: Duration = Duration::from_millis(20); // sleep-ok
#[cfg(unix)]
const STOP_NONE: u8 = 0;
#[cfg(unix)]
const STOP_NOW: u8 = 1;
#[cfg(unix)]
const STOP_DRAIN: u8 = 2;
#[cfg(unix)]
const DRAIN_MAX_DURATION: Duration = Duration::from_millis(200);
#[cfg(unix)]
const DRAIN_MAX_BYTES: usize = 512 * 1024;

type Responder = Box<dyn FnMut(&[u8]) -> Option<Vec<u8>> + Send>;

/// Builder for launching `mare` (or a shell wrapper around it) in a temp `MARE_HOME` + workspace.
pub struct MareCmd {
    home: Option<tempfile::TempDir>,
    home_path: PathBuf,
    work: tempfile::TempDir,
    args: Vec<String>,
    env: Vec<(OsString, OsString)>,
    faux: Option<Value>,
    setup: String,
    init_tail: String,
    write_init: bool,
    plugins: Vec<(String, String)>,
    reports: usize,
    shell_script: Option<(String, Vec<String>)>,
    responder: Option<Responder>,
}

impl MareCmd {
    pub fn new() -> Self {
        Self::new_with_env(std::env::vars_os())
    }

    fn new_with_env<I>(vars: I) -> Self
    where
        I: IntoIterator<Item = (OsString, OsString)>,
    {
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path().to_path_buf();
        let work = tempfile::tempdir().unwrap();
        let tmp = home_path.join("tmp");
        std::fs::create_dir(&tmp).unwrap();

        let mut env: Vec<(OsString, OsString)> = vars
            .into_iter()
            .filter(|(k, _)| {
                let Some(s) = k.to_str() else { return true };
                (matches!(s, "MARE_TEST_OFFLINE" | "MARE_TEST_ALLOW_LIVE") || !s.starts_with("MARE_"))
                    && !matches!(
                        s,
                        "TERM"
                            | "TERM_PROGRAM"
                            | "KITTY_WINDOW_ID"
                            | "GHOSTTY_RESOURCES_DIR"
                            | "WEZTERM_PANE"
                            | "WARP_SESSION_ID"
                            | "ITERM_SESSION_ID"
                            | "ZELLIJ"
                    )
            })
            .collect();
        env.push(("MARE_HOME".into(), home_path.as_os_str().into()));
        let allow_live = env.iter().any(|(k, v)| k == "MARE_TEST_ALLOW_LIVE" && v == "1");
        if !allow_live {
            env.retain(|(key, _)| key != "MARE_TEST_OFFLINE");
            env.push(("MARE_TEST_OFFLINE".into(), "1".into()));
        }
        for name in ["TMPDIR", "TMP", "TEMP"] {
            env.retain(|(key, _)| key != name);
            env.push((name.into(), tmp.as_os_str().into()));
        }
        env.retain(|(key, _)| key != "BROWSER");
        env.push(("BROWSER".into(), "true".into()));
        env.push(("TERM".into(), "xterm-256color".into()));

        Self {
            home: Some(home),
            home_path,
            work,
            args: Vec::new(),
            env,
            faux: None,
            setup: String::new(),
            init_tail: String::new(),
            write_init: true,
            plugins: Vec::new(),
            reports: 0,
            shell_script: None,
            responder: None,
        }
    }

    pub fn home_path(&self) -> &Path {
        &self.home_path
    }

    pub fn work_path(&self) -> &Path {
        self.work.path()
    }

    /// Reuse an existing `MARE_HOME` directory (the caller owns its lifetime).
    pub fn home_dir(mut self, path: impl AsRef<Path>) -> Self {
        let p = path.as_ref().to_path_buf();
        let tmp = p.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        self.home = None;
        self.home_path = p.clone();
        let mut this = self.env("MARE_HOME", &p);
        for name in ["TMPDIR", "TMP", "TEMP"] {
            this = this.env(name, &tmp);
        }
        this
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn faux(mut self, script: Value) -> Self {
        self.faux = Some(script);
        self
    }

    pub fn setup(mut self, lua: &str) -> Self {
        if !self.setup.is_empty() && !lua.is_empty() {
            self.setup.push_str(", ");
        }
        self.setup.push_str(lua);
        self
    }

    pub fn init_tail(mut self, lua: &str) -> Self {
        self.init_tail.push_str(lua);
        if !lua.ends_with('\n') {
            self.init_tail.push('\n');
        }
        self
    }

    pub fn no_init(mut self) -> Self {
        self.write_init = false;
        self
    }

    pub fn plugin(mut self, name: &str, src: &str) -> Self {
        self.plugins.push((name.to_string(), src.to_string()));
        self
    }

    /// Listen to `event` via the public `mare.on` API and write its JSON payload to a temp file.
    pub fn report(mut self, event: &str) -> (Self, PathBuf) {
        let path = self.home_path.join(format!("report-{}.json", self.reports));
        self.reports += 1;
        let ev_lit = serde_json::to_string(event).unwrap();
        let path_lit = serde_json::to_string(&path.to_string_lossy().as_ref()).unwrap();
        let lua = format!(
            "mare.on({ev_lit}, function(ev) mare.fs.write({path_lit}, mare.json.encode(ev or {{}})); return ev end)\n"
        );
        self = self.init_tail(&lua);
        (self, path)
    }

    pub fn env(mut self, k: &str, v: impl AsRef<OsStr>) -> Self {
        self.env.retain(|(name, _)| name.to_str() != Some(k));
        self.env.push((k.into(), v.as_ref().to_os_string()));
        self
    }

    pub fn env_remove(mut self, k: &str) -> Self {
        self.env.retain(|(name, _)| name.to_str() != Some(k));
        self
    }

    #[cfg(test)]
    pub(crate) fn env_value(&self, k: &str) -> Option<&OsStr> {
        self.env.iter().find(|(name, _)| name.to_str() == Some(k)).map(|(_, v)| v.as_os_str())
    }

    /// Disable the startup terminal probe (for tests that do not answer it).
    pub fn no_term_probe(self) -> Self {
        self.env("MARE_NO_TERM_PROBE", "1")
    }

    /// Run a shell command instead of `mare` directly (e.g. `sh -c '"$0"; stty -a > "$1"'`).
    pub fn shell_wrap(mut self, prog: &str, args: Vec<String>) -> Self {
        self.shell_script = Some((prog.to_string(), args));
        self
    }

    /// Register a probe responder to run in the PTY reader thread as bytes arrive.
    pub fn respond(mut self, f: impl FnMut(&[u8]) -> Option<Vec<u8>> + Send + 'static) -> Self {
        self.responder = Some(Box::new(f));
        self
    }

    pub fn pty(mut self, cols: u16, rows: u16) -> PtySession {
        if self.write_init {
            let cfg = self.home_path.join("config");
            std::fs::create_dir_all(&cfg).unwrap();
            let setup = &self.setup;
            let tail = &self.init_tail;
            std::fs::write(
                cfg.join("init.lua"),
                format!("mare.setup {{ model = \"faux/echo\", logo = false, {setup} }}\n{tail}"),
            )
            .unwrap();
        }
        if !self.plugins.is_empty() {
            let dir = self.home_path.join("config").join("plugins");
            std::fs::create_dir_all(&dir).unwrap();
            for (name, src) in &self.plugins {
                std::fs::write(dir.join(format!("{name}.lua")), src).unwrap();
            }
        }
        if let Some(script) = self.faux.take() {
            let faux_path = self.home_path.join("faux.json");
            std::fs::write(&faux_path, serde_json::to_vec(&script).unwrap()).unwrap();
            self.env.retain(|(name, _)| name != "MARE_FAUX_SCRIPT");
            self.env.push(("MARE_FAUX_SCRIPT".into(), faux_path.into_os_string()));
        }

        let mare = super::mare_pty_program(&mut self.env);
        let (prog, args) = match self.shell_script.take() {
            Some((p, a)) => (p, a),
            None => {
                let mut args = std::mem::take(&mut self.args);
                if !args.iter().any(|a| a.contains("onboarding.plugins")) {
                    args.push("--set".into());
                    args.push("plugins.onboarding.plugins=false".into());
                }
                (mare.to_string(), args)
            }
        };
        let cwd = self.work.path().to_string_lossy().into_owned();
        let (term, mut reader, writer) =
            Terminal::spawn(TerminalCommand { prog: &prog, args: &args, cwd: &cwd, env: self.env, cols, rows })
                .unwrap();

        let pid = term.pid();
        let out = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::new(Mutex::new(Some(writer)));
        let responder = Arc::new(Mutex::new(self.responder.take()));

        let sink = out.clone();
        let writer_ref = writer.clone();
        let resp_ref = responder.clone();

        #[cfg(unix)]
        let stop = Arc::new(AtomicU8::new(STOP_NONE));
        #[cfg(unix)]
        let stop2 = stop.clone();
        #[cfg(unix)]
        let fd = term.as_raw_fd();

        #[cfg(unix)]
        let reader_thread = std::thread::spawn(move || {
            // Poll with a timeout (not a blocking read) so `hang_up` can stop this thread
            // immediately without draining, and `wait_exit` can drain buffered bytes with a
            // strict time/byte bound before closing the master fd.
            let mut buf = [0u8; 8192];
            let mut drain_started: Option<Instant> = None;
            let mut drained_bytes: usize = 0;
            loop {
                let mode = stop2.load(Ordering::Acquire);
                if mode == STOP_NOW {
                    break;
                }
                if mode == STOP_DRAIN {
                    let started = *drain_started.get_or_insert_with(Instant::now);
                    if started.elapsed() >= DRAIN_MAX_DURATION || drained_bytes >= DRAIN_MAX_BYTES {
                        break;
                    }
                }
                let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                // SAFETY: one valid pollfd for the master fd, which the terminal keeps open until
                // a hangup has stopped this thread.
                let n = unsafe { libc::poll(&mut pfd, 1, if mode == STOP_NONE { 50 } else { 0 }) };
                if n < 0 {
                    break;
                }
                let mode = stop2.load(Ordering::Acquire);
                if mode == STOP_NOW {
                    break;
                }
                if n == 0 {
                    if mode == STOP_DRAIN {
                        break;
                    }
                    continue;
                }
                if mode == STOP_DRAIN && (pfd.revents & libc::POLLIN) == 0 {
                    break;
                }
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if mode == STOP_DRAIN {
                            drained_bytes = drained_bytes.saturating_add(n);
                        }
                        let snapshot = {
                            let mut guard = sink.lock().unwrap();
                            guard.extend_from_slice(&buf[..n]);
                            guard.clone()
                        };
                        let reply = resp_ref.lock().unwrap().as_mut().and_then(|f| f(&snapshot));
                        if let Some(bytes) = reply
                            && let Some(w) = writer_ref.lock().unwrap().as_mut()
                        {
                            let _ = w.write_all(&bytes);
                            let _ = w.flush();
                        }
                    }
                }
            }
        });

        #[cfg(windows)]
        let reader_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let snapshot = {
                            let mut guard = sink.lock().unwrap();
                            guard.extend_from_slice(&buf[..n]);
                            guard.clone()
                        };
                        let reply = resp_ref.lock().unwrap().as_mut().and_then(|f| f(&snapshot));
                        if let Some(bytes) = reply
                            && let Some(w) = writer_ref.lock().unwrap().as_mut()
                        {
                            let _ = w.write_all(&bytes);
                            let _ = w.flush();
                        }
                    }
                }
            }
        });

        PtySession {
            term: Some(term),
            pid,
            exited: None,
            exit_code: None,
            writer,
            out,
            responder,
            #[cfg(unix)]
            stop,
            reader: Some(reader_thread),
            home: self.home,
            home_path: self.home_path,
            work: self.work,
            cols,
            rows,
        }
    }
}

pub struct PtySession {
    term: Option<Terminal>,
    pid: u32,
    exited: Option<bool>,
    exit_code: Option<u32>,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    out: Arc<Mutex<Vec<u8>>>,
    responder: Arc<Mutex<Option<Responder>>>,
    #[cfg(unix)]
    stop: Arc<AtomicU8>,
    reader: Option<std::thread::JoinHandle<()>>,
    home: Option<tempfile::TempDir>,
    home_path: PathBuf,
    work: tempfile::TempDir,
    cols: u16,
    rows: u16,
}

impl PtySession {
    pub fn home_path(&self) -> &Path {
        &self.home_path
    }

    pub fn work_path(&self) -> &Path {
        self.work.path()
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.out.lock().unwrap().clone()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes()).into_owned()
    }

    pub fn screen(&self) -> String {
        render_screen(&self.bytes(), self.cols, self.rows)
    }

    pub fn respond(&mut self, f: impl FnMut(&[u8]) -> Option<Vec<u8>> + Send + 'static) {
        *self.responder.lock().unwrap() = Some(Box::new(f));
        let snapshot = self.bytes();
        let reply = self.responder.lock().unwrap().as_mut().and_then(|cb| cb(&snapshot));
        if let Some(bytes) = reply {
            self.send(&bytes);
        }
    }

    pub fn wait_file(&self, p: &Path) -> Value {
        let end = Instant::now() + Duration::from_secs(WAIT_SECS);
        loop {
            if let Ok(raw) = std::fs::read_to_string(p)
                && let Ok(val) = serde_json::from_str::<Value>(&raw)
            {
                return val;
            }
            assert!(
                Instant::now() < end,
                "timed out waiting for file {}; screen:\n{}\nraw tail: {:?}",
                p.display(),
                self.screen(),
                tail(&self.text(), 600)
            );
            std::thread::sleep(POLL_INTERVAL); // sleep-ok: poll interval waiting for JSON report file
        }
    }

    pub fn wait_text(&self, what: &str, f: impl Fn(&str) -> bool) {
        let end = Instant::now() + Duration::from_secs(WAIT_SECS);
        while !f(&self.text()) {
            assert!(Instant::now() < end, "timed out waiting for {what}; output tail: {:?}", tail(&self.text(), 600));
            std::thread::sleep(POLL_INTERVAL); // sleep-ok: poll interval waiting for PTY text predicate
        }
    }

    pub fn wait_screen(&self, what: &str, f: impl Fn(&str) -> bool) {
        let end = Instant::now() + Duration::from_secs(WAIT_SECS);
        while !f(&self.screen()) {
            assert!(Instant::now() < end, "timed out waiting for {what}; screen:\n{}", self.screen());
            std::thread::sleep(POLL_INTERVAL); // sleep-ok: poll interval waiting for PTY screen predicate
        }
    }

    pub fn wait_raw(&self, what: &str, f: impl Fn(&[u8]) -> bool) {
        let end = Instant::now() + Duration::from_secs(WAIT_SECS);
        while !f(&self.bytes()) {
            assert!(Instant::now() < end, "timed out waiting for {what}; output tail: {:?}", tail(&self.text(), 600));
            std::thread::sleep(POLL_INTERVAL); // sleep-ok: poll interval waiting for raw PTY bytes predicate
        }
    }

    /// Type `text` and press Enter until `sent` shows: Enter while startup is still in progress
    /// sends nothing and keeps the text in the input box, so it is
    /// pressed again until the prompt goes.
    pub fn prompt(&mut self, text: &str, what: &str, sent: impl Fn(&str) -> bool) {
        self.send(text.as_bytes());
        if cfg!(windows) && !text.is_empty() {
            // Windows enables unbracketed PasteBurst (120ms enter suppress window); pause well
            // past it so ConPTY input delivery lag cannot turn Enter into a newline.
            // sleep-ok: separates synthetic keystroke input from Enter past the PasteBurst window
            std::thread::sleep(Duration::from_millis(250));
        }
        let end = Instant::now() + Duration::from_secs(WAIT_SECS);
        loop {
            self.send(b"\r");
            let retry = Instant::now() + Duration::from_millis(500);
            while Instant::now() < retry {
                if sent(&self.text()) {
                    return;
                }
                std::thread::sleep(POLL_INTERVAL); // sleep-ok: poll interval waiting for prompt submission
            }
            assert!(Instant::now() < end, "timed out waiting for {what}; output tail: {:?}", tail(&self.text(), 600));
        }
    }

    pub fn send(&mut self, bytes: &[u8]) {
        let mut guard = self.writer.lock().unwrap();
        let w = guard.as_mut().expect("terminal still open");
        w.write_all(bytes).unwrap();
        w.flush().unwrap();
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        if let Some(t) = &self.term {
            t.resize(cols, rows).unwrap();
        }
    }

    /// The terminal goes away: every master fd is closed (the kernel hangs up the session).
    pub fn hang_up(&mut self) {
        self.stop_and_close(true);
    }

    fn stop_and_close(&mut self, immediate: bool) {
        #[cfg(unix)]
        {
            self.stop.store(if immediate { STOP_NOW } else { STOP_DRAIN }, Ordering::Release);
            if let Some(t) = self.reader.take() {
                let _ = t.join();
            }
            self.writer.lock().unwrap().take();
            if let Some(t) = &mut self.term {
                t.hang_up();
            }
        }
        #[cfg(windows)]
        {
            let _ = immediate;
            self.writer.lock().unwrap().take();
            self.term.take();
            if let Some(t) = self.reader.take() {
                let _ = t.join();
            }
        }
    }

    pub fn running(&mut self) -> bool {
        self.try_exit().is_none()
    }

    pub fn try_exit(&mut self) -> Option<bool> {
        if self.exited.is_none() {
            match &mut self.term {
                Some(t) => {
                    if let Some(code) = t.try_wait().unwrap() {
                        self.exit_code = Some(code);
                        self.exited = Some(code == 0);
                    }
                }
                #[cfg(windows)]
                None => {
                    if !mare_binding::platform::tree_alive(Some(self.pid)) {
                        self.exit_code = Some(0);
                        self.exited = Some(true);
                    }
                }
                #[cfg(unix)]
                None => unreachable!("unix keeps Terminal after hang_up to query mare-pty-wait"),
            }
        }
        self.exited
    }

    pub fn wait_exit(&mut self, secs: u64) -> bool {
        let _ = self.wait_exit_code(secs);
        self.exited.unwrap_or(false)
    }

    pub fn wait_exit_code(&mut self, secs: u64) -> Option<u32> {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            if self.try_exit().is_some() {
                self.stop_and_close(false);
                return self.exit_code;
            }
            if Instant::now() > end {
                if let Some(t) = &mut self.term {
                    let _ = t.kill();
                } else {
                    mare_binding::platform::signal_tree(Some(self.pid), mare_binding::platform::Signal::Kill);
                }
                panic!("mare did not exit; output tail: {:?}", tail(&self.text(), 600));
            }
            std::thread::sleep(POLL_INTERVAL); // sleep-ok: poll interval waiting for child exit
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self.exited.is_none() {
            if let Some(term) = &mut self.term {
                if let Err(error) = term.kill() {
                    eprintln!("PTY fixture kill {}: {error}", self.pid);
                }
                // On Windows, `hang_up()` drops `self.term`; wait (bounded) for the killed child
                // before closing ConPTY so the process releases directory handles first.
                #[cfg(windows)]
                {
                    let _ = term.wait_timeout(Duration::from_secs(3));
                }
            } else {
                mare_binding::platform::signal_tree(Some(self.pid), mare_binding::platform::Signal::Kill);
            }
        }
        self.hang_up();
        #[cfg(unix)]
        if self.exited.is_none()
            && let Some(term) = &mut self.term
        {
            let _ = term.wait_timeout(Duration::from_secs(3));
        }
        // Release the ConPTY / PTY handle before removing temporary directories so Windows does
        // not report ERROR_SHARING_VIOLATION (os error 32) on an open child handle.
        self.term = None;
        for dir in [self.home.as_ref().map(|d| d.path()), Some(self.work.path())].into_iter().flatten() {
            if let Err(error) = remove_dir_all_with_retry(dir) {
                let message = format!("PTY fixture cleanup {}: {error}", dir.display());
                if std::thread::panicking() {
                    eprintln!("{message}");
                } else {
                    panic!("{message}");
                }
            }
        }
    }
}

fn remove_dir_all_with_retry(dir: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match std::fs::remove_dir_all(dir) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e)
                    if (e.kind() == std::io::ErrorKind::PermissionDenied
                        || matches!(e.raw_os_error(), Some(5 | 32 | 145)))
                        && std::time::Instant::now() < deadline =>
                {
                    // sleep-ok: Windows ConPTY / child process handle release backoff during PTY fixture cleanup
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(e),
            }
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::remove_dir_all(dir)
    }
}

pub fn render_screen(bytes: &[u8], cols: u16, rows: u16) -> String {
    let cols = cols.max(1);
    let mut p = vt100::Parser::new(
        std::num::NonZeroU16::new(rows.max(1)).unwrap(),
        std::num::NonZeroU16::new(cols).unwrap(),
        0,
    );
    p.process(bytes);
    // Read physical rows rather than `Screen::contents`: Windows ConPTY omits `\r\n` after
    // full-width lines (such as the 100-column input box borders) and clears blank rows with spaces,
    // which causes `Screen::contents` to glue consecutive visual rows and drop blank breaks.
    let mut lines: Vec<String> = p.screen().rows(0, cols).map(|row| row.trim_end().to_owned()).collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

pub fn tail(s: &str, n: usize) -> String {
    let start = s.len().saturating_sub(n);
    let start = (start..s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(s.len());
    s[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mare_cmd_preserves_and_injects_test_offline() {
        let cmd = MareCmd::new();
        assert_eq!(cmd.env_value("MARE_TEST_OFFLINE"), Some(OsStr::new("1")));
        assert_eq!(cmd.env_value("BROWSER"), Some(OsStr::new("true")));

        let explicit = MareCmd::new().env("MARE_TEST_OFFLINE", "custom");
        assert_eq!(explicit.env_value("MARE_TEST_OFFLINE"), Some(OsStr::new("custom")));

        let removed = MareCmd::new().env_remove("MARE_TEST_OFFLINE");
        assert_eq!(removed.env_value("MARE_TEST_OFFLINE"), None);

        let forced = MareCmd::new_with_env([
            (OsString::from("MARE_TEST_OFFLINE"), OsString::from("0")),
            (OsString::from("MARE_UNRELATED"), OsString::from("drop-me")),
        ]);
        assert_eq!(forced.env_value("MARE_TEST_OFFLINE"), Some(OsStr::new("1")));
        assert_eq!(forced.env_value("MARE_UNRELATED"), None);

        let live = MareCmd::new_with_env([(OsString::from("MARE_TEST_ALLOW_LIVE"), OsString::from("1"))]);
        assert_eq!(live.env_value("MARE_TEST_ALLOW_LIVE"), Some(OsStr::new("1")));
        assert_eq!(live.env_value("MARE_TEST_OFFLINE"), None);
    }

    // Adapted from Codex `codex-rs/utils/pty/src/tests.rs:1104`
    #[cfg(unix)]
    #[test]
    fn hang_up_and_wait_exit_are_bounded_under_continuous_output() {
        // 1. Active hang_up while child continuously writes to PTY.
        let mut p = MareCmd::new()
            .no_init()
            .shell_wrap(
                "sh",
                vec![
                    "-c".to_string(),
                    "printf 'READY\\n'; exec yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
                        .to_string(),
                ],
            )
            .pty(80, 24);
        p.wait_raw("continuous x output", |b| b.windows(16).any(|w| w == b"xxxxxxxxxxxxxxxx"));

        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            p.hang_up();
            let _ = tx.send(p);
        });
        let mut p = rx.recv_timeout(Duration::from_secs(2)).expect("hang_up hung under continuous PTY output");
        handle.join().unwrap();
        let _ = p.wait_exit(5);

        // 2. Post-exit drain when a background descendant keeps writing to PTY slave.
        let mut p2 = MareCmd::new()
            .no_init()
            .shell_wrap(
                "sh",
                vec![
                    "-c".to_string(),
                    "(trap '' HUP; touch ready; exec yes yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy) & while [ ! -f ready ]; do :; done; printf 'PARENT-EXIT\\n'"
                        .to_string(),
                ],
            )
            .pty(80, 24);
        p2.wait_raw("PARENT-EXIT", |b| b.windows(11).any(|w| w == b"PARENT-EXIT"));

        let (tx2, rx2) = std::sync::mpsc::channel();
        let handle2 = std::thread::spawn(move || {
            let _ = p2.wait_exit(5);
            let _ = tx2.send(p2);
        });
        let p2 =
            rx2.recv_timeout(Duration::from_secs(3)).expect("wait_exit hung draining continuous descendant PTY output");
        handle2.join().unwrap();
        drop(p2);
    }
}
