//! External output capture.
//!
//! While the TUI owns the terminal, anything else in the mare process that writes to stdout or
//! stderr (a plugin's `print`, a library's `eprintln!`, a child spawned with inherited stdio)
//! would scribble over the live area. On Unix, fds 1 and 2 are pointed at a pipe for that time;
//! the renderer keeps writing to the real terminal through the saved fd ([`tty_fd`]). Captured
//! bytes become lines — `\r` resets the current line (a spinner doesn't pile up), `\n` commits —
//! go through the untrusted-text filter (SGR kept as styled spans), wait in a bounded queue, and
//! are committed into scrollback at most [`PER_FRAME`] lines per frame, so a flood never turns
//! into one huge frame. Handing the terminal to another program (editor, Ctrl+Z, exit) restores
//! fds 1 and 2 first ([`pause`] / [`resume`]).
//!
//! Only output mare can intercept is covered: programs that open the terminal device directly
//! are not (`mare doctor` says so).

use std::collections::VecDeque;
use std::sync::Mutex;

/// Lines committed per frame (OpenTUI).
pub const PER_FRAME: usize = 8;
/// Queue bound (lines); beyond it the oldest lines are replaced by a count.
pub const MAX_QUEUED: usize = 10_000;
/// Longest captured line kept (bytes); the rest of a runaway line is cut.
const MAX_LINE: usize = 16 * 1024;

/// Splits captured bytes into lines: `\r\n` and `\n` commit, a lone `\r` resets the line.
#[derive(Default)]
pub struct LineSplitter {
    cur: Vec<u8>,
    cr: bool,
    flushed_idle: bool,
}

impl LineSplitter {
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<String>) {
        for &b in bytes {
            match b {
                b'\n' => {
                    self.cr = false;
                    if self.flushed_idle && self.cur.is_empty() {
                        self.flushed_idle = false;
                        continue;
                    }
                    self.flushed_idle = false;
                    out.push(Self::finish_line(std::mem::take(&mut self.cur)));
                }
                b'\r' => self.cr = true,
                _ => {
                    self.flushed_idle = false;
                    if self.cr {
                        // a lone CR: the next text overwrites the line (spinners, progress)
                        self.cr = false;
                        self.cur.clear();
                    }
                    if self.cur.len() < MAX_LINE {
                        self.cur.push(b);
                    }
                }
            }
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.cur.is_empty()
    }

    /// Idle flush: emit an incomplete line (e.g. `Password: `) without waiting for `\n` or EOF.
    pub fn flush_idle(&mut self, out: &mut Vec<String>) {
        if !self.cur.is_empty() {
            self.cr = false;
            self.flushed_idle = true;
            out.push(Self::finish_line(std::mem::take(&mut self.cur)));
        }
    }

    /// End of stream: whatever is left is a line.
    pub fn flush(&mut self, out: &mut Vec<String>) {
        self.cr = false;
        self.flushed_idle = false;
        if !self.cur.is_empty() {
            out.push(Self::finish_line(std::mem::take(&mut self.cur)));
        }
    }

    fn finish_line(raw: Vec<u8>) -> String {
        // untrusted: every control sequence but SGR goes; SGR becomes spans later
        mare_shared::ansi::sanitize(&String::from_utf8_lossy(&raw), true)
    }
}

/// Bounded FIFO of captured lines.
#[derive(Default)]
pub struct Queue {
    lines: VecDeque<String>,
    dropped: usize,
}

impl Queue {
    pub fn push(&mut self, line: String) {
        if self.lines.len() >= MAX_QUEUED {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(line);
    }

    /// Up to `n` lines for this frame (preceded by a note if lines were dropped meanwhile).
    pub fn take(&mut self, n: usize) -> Vec<String> {
        let mut out = Vec::new();
        if self.dropped > 0 {
            out.push(format!("… {} earlier output lines dropped (queue full)", std::mem::take(&mut self.dropped)));
        }
        let k = n.saturating_sub(out.len()).min(self.lines.len());
        out.extend(self.lines.drain(..k));
        out
    }

    pub fn len(&self) -> usize {
        self.lines.len() + usize::from(self.dropped > 0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

static QUEUE: Mutex<Option<Queue>> = Mutex::new(None);
static NOTIFY: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();

fn notify() -> &'static tokio::sync::Notify {
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}

#[cfg_attr(not(unix), allow(dead_code))]
fn push_lines(lines: Vec<String>) {
    if lines.is_empty() {
        return;
    }
    {
        let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        let q = q.get_or_insert_with(Queue::default);
        for l in lines {
            q.push(l);
        }
    }
    notify().notify_one();
}

/// Lines for the next frame (at most [`PER_FRAME`]).
pub fn take_frame() -> Vec<String> {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).as_mut().map(|q| q.take(PER_FRAME)).unwrap_or_default()
}

/// Everything still queued (exit).
pub fn take_all() -> Vec<String> {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).as_mut().map(|q| q.take(usize::MAX)).unwrap_or_default()
}

pub fn pending() -> usize {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |q| q.len())
}

/// Resolves when captured output arrived.
pub async fn arrived() {
    notify().notified().await
}

// ---- fd redirection (Unix) -------------------------------------------------------------------

#[cfg(unix)]
mod fds {
    use std::sync::atomic::{AtomicI32, Ordering};

    /// dup of the real terminal fd (the renderer writes here while capturing); -1 = not capturing
    pub static TTY: AtomicI32 = AtomicI32::new(-1);
    /// write end of the capture pipe (kept to re-point fds 1/2 after a handoff)
    pub static PIPE_W: AtomicI32 = AtomicI32::new(-1);

    pub fn point(fd: i32) {
        // SAFETY: plain dup2 on fds we own; failure leaves the old target
        unsafe {
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
        }
    }

    pub fn start() -> std::io::Result<()> {
        if TTY.load(Ordering::SeqCst) >= 0 {
            return Ok(());
        }
        // SAFETY: fd syscalls; every fd created here is either stored or closed
        unsafe {
            if libc::isatty(1) != 1 {
                return Ok(()); // stdout isn't the terminal: nothing to protect
            }
            let tty = libc::dup(1);
            if tty < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut p = [0i32; 2];
            if libc::pipe(p.as_mut_ptr()) != 0 {
                libc::close(tty);
                return Err(std::io::Error::last_os_error());
            }
            libc::fcntl(p[0], libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(p[1], libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(tty, libc::F_SETFD, libc::FD_CLOEXEC);
            TTY.store(tty, Ordering::SeqCst);
            PIPE_W.store(p[1], Ordering::SeqCst);
            point(p[1]);
            let r = p[0];
            std::thread::Builder::new().name("mare-capture".into()).spawn(move || {
                read_loop(r, 100, super::push_lines);
                libc::close(r);
            })?;
        }
        Ok(())
    }

    pub(super) fn read_loop(r: i32, idle_timeout_ms: i32, mut emit: impl FnMut(Vec<String>)) {
        let mut split = super::LineSplitter::default();
        let mut buf = [0u8; 8192];
        loop {
            if split.has_pending() {
                let mut pfd = libc::pollfd { fd: r, events: libc::POLLIN, revents: 0 };
                // SAFETY: pfd points to one valid pollfd for an open pipe fd.
                let ready = unsafe { libc::poll(&mut pfd, 1, idle_timeout_ms) };
                if ready == 0 {
                    let mut lines = vec![];
                    split.flush_idle(&mut lines);
                    if !lines.is_empty() {
                        emit(lines);
                    }
                    continue;
                }
                if ready < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
            }
            // SAFETY: r is a valid open pipe read fd and buf is writable.
            let n = unsafe { libc::read(r, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if n == 0 {
                break;
            }
            let mut lines = vec![];
            split.feed(&buf[..n as usize], &mut lines);
            if !lines.is_empty() {
                emit(lines);
            }
        }
        let mut lines = vec![];
        split.flush(&mut lines);
        if !lines.is_empty() {
            emit(lines);
        }
    }
}

/// Start capturing (TUI startup). No-op when stdout isn't a terminal or on Windows.
pub fn start() {
    #[cfg(unix)]
    if let Err(e) = fds::start() {
        tracing::warn!("stdout/stderr capture unavailable: {e}");
    }
}

/// Hand the terminal to another program: fds 1/2 go back to the terminal.
pub fn pause() {
    #[cfg(unix)]
    {
        let tty = fds::TTY.load(std::sync::atomic::Ordering::SeqCst);
        if tty >= 0 {
            fds::point(tty);
        }
    }
}

/// Back from a handoff: capture again.
pub fn resume() {
    #[cfg(unix)]
    {
        let w = fds::PIPE_W.load(std::sync::atomic::Ordering::SeqCst);
        if w >= 0 {
            fds::point(w);
        }
    }
}

/// The fd the renderer writes to: the saved terminal while capturing, stdout otherwise.
pub fn tty_fd() -> Option<i32> {
    #[cfg(unix)]
    {
        let t = fds::TTY.load(std::sync::atomic::Ordering::SeqCst);
        if t >= 0 {
            return Some(t);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]]) -> Vec<String> {
        let mut s = LineSplitter::default();
        let mut out = vec![];
        for c in chunks {
            s.feed(c, &mut out);
        }
        s.flush(&mut out);
        out
    }

    #[test]
    fn cr_resets_the_line_and_newline_commits() {
        assert_eq!(split(&[b"one\ntwo\r\nthree"]), ["one", "two", "three"]);
        // a spinner rewriting one line ends as its last state, not a pile of frames
        assert_eq!(split(&[b"[1/3]\r[2/3]\r", b"[3/3] done\n"]), ["[3/3] done"]);
        // split across chunks, including the CR LF pair
        assert_eq!(split(&[b"ab", b"c\r", b"\nd"]), ["abc", "d"]);
    }

    #[test]
    fn hostile_sequences_are_dropped_but_colors_kept() {
        let got = split(&[b"\x1b]52;c;aGk=\x07\x1b[2Jhi \x1b[31mred\x1b[0m\n"]);
        assert_eq!(got.len(), 1);
        assert!(!got[0].contains("]52") && !got[0].contains("[2J"), "{got:?}");
        assert!(got[0].contains("\x1b[31m"), "SGR kept for styling: {got:?}");
    }

    #[test]
    fn at_most_eight_lines_per_frame_and_a_bounded_queue() {
        let mut q = Queue::default();
        for i in 0..20 {
            q.push(format!("l{i}"));
        }
        assert_eq!(q.take(PER_FRAME).len(), 8);
        assert_eq!(q.take(PER_FRAME).len(), 8);
        assert_eq!(q.take(PER_FRAME).len(), 4);
        for i in 0..MAX_QUEUED + 3 {
            q.push(format!("x{i}"));
        }
        let first = q.take(2);
        assert!(first[0].contains("3 earlier output lines dropped"), "{first:?}");
        assert_eq!(first[1], "x3");
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_line_without_newline_flushes_after_idle_timeout() {
        let mut pipe = [0i32; 2];
        // SAFETY: pipe writes two valid file descriptors.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let (r, w) = (pipe[0], pipe[1]);
        let (tx, rx) = std::sync::mpsc::channel::<Vec<String>>();
        let handle = std::thread::spawn(move || {
            fds::read_loop(r, 40, |lines| {
                let _ = tx.send(lines);
            });
            // SAFETY: r is owned by this reader thread.
            unsafe { libc::close(r) };
        });
        let msg = b"Password: ";
        // SAFETY: w is a valid open write end of the pipe.
        assert_eq!(unsafe { libc::write(w, msg.as_ptr().cast(), msg.len()) }, msg.len() as isize);
        // Keep `w` open (no EOF, no `\n`): prompt must flush after idle timeout.
        let got = rx.recv_timeout(std::time::Duration::from_millis(300));
        // SAFETY: closing w lets the reader thread exit cleanly.
        unsafe { libc::close(w) };
        handle.join().unwrap();
        assert_eq!(got.ok(), Some(vec!["Password: ".to_string()]));
    }
}
