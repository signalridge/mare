//! The process terminal through termina: raw mode, the window size and the input event
//! stream. Output never goes through here: every byte goes out through the single write exit
//! (`out`), and the modes mare turns on are its own bytes (`modes`).
//!
//! The terminal is opened once for the TUI and kept until it ends (`close`); handoffs (external
//! editor, Ctrl+Z) only switch it back to cooked mode and raw again.
//!
//! [`Events`] is mare's own async adapter over termina's blocking reader instead of termina's
//! `EventStream`, whose helper thread retries a failed read forever and never wakes the task: the
//! EOF / EIO of a terminal that disappeared without a SIGHUP must reach the event loop, which
//! quits on it like on SIGHUP. Like termina's own stream, the reader only waits on
//! the terminal while the loop asks for input, so a program the terminal is handed to keeps its
//! keys; and whichever task polled last is the one woken (a `now_or_never` poll can't swallow the
//! wakeup).

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

pub use termina::WindowSize;
pub use termina::event::{KeyCode, KeyEventKind, KeyEventState, Modifiers, MouseButton, MouseEvent, MouseEventKind};

/// Terminal key event with optional Kitty `base_layout_key` (`CSI code:shifted:base;mods u`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: Modifiers,
    pub kind: KeyEventKind,
    pub state: KeyEventState,
    pub base_layout_key: Option<char>,
}

impl KeyEvent {
    /// Construct a press key event with no extra state or alternate layout key.
    pub const fn new(code: KeyCode, modifiers: Modifiers) -> Self {
        Self { code, modifiers, kind: KeyEventKind::Press, state: KeyEventState::NONE, base_layout_key: None }
    }

    /// Attach a Kitty base layout key to this event.
    pub const fn with_base_layout_key(mut self, base_layout_key: Option<char>) -> Self {
        self.base_layout_key = base_layout_key;
        self
    }
}

impl From<termina::event::KeyEvent> for KeyEvent {
    fn from(ev: termina::event::KeyEvent) -> Self {
        Self { code: ev.code, modifiers: ev.modifiers, kind: ev.kind, state: ev.state, base_layout_key: None }
    }
}

/// Terminal event wrapping `termina::Event` so `Event::Key` carries [`KeyEvent`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    WindowResized(WindowSize),
    Paste(String),
    FocusIn,
    FocusOut,
    Termina(termina::Event),
}

/// A terminal event stamped with its arrival timestamp when read by `mare-input`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimedEvent {
    pub event: Event,
    pub at: std::time::Instant,
}

impl Event {
    /// Whether this event carries a raw terminal escape reply (DA1/DECRPM/OSC).
    pub fn is_escape(&self) -> bool {
        matches!(self, Self::Termina(e) if e.is_escape())
    }

    /// 1-based terminal row when this event is a Cursor Position Report (`CSI row;col R`).
    pub fn cursor_position_row(&self) -> Option<usize> {
        match self {
            Self::Termina(termina::Event::Csi(termina::escape::csi::Csi::Cursor(
                termina::escape::csi::Cursor::ActivePositionReport { line, .. },
            ))) => Some(line.get() as usize),
            _ => None,
        }
    }

    /// Construct a 1-based Cursor Position Report event (`CSI line;col R`).
    pub fn cursor_position_report(line: u16, col: u16) -> Self {
        Self::Termina(termina::Event::Csi(termina::escape::csi::Csi::Cursor(
            termina::escape::csi::Cursor::ActivePositionReport {
                line: termina::OneBased::new(line.max(1)).expect("non-zero"),
                col: termina::OneBased::new(col.max(1)).expect("non-zero"),
            },
        )))
    }
}

impl From<termina::Event> for Event {
    fn from(ev: termina::Event) -> Self {
        match ev {
            termina::Event::Key(k) => Self::Key(KeyEvent::from(k)),
            termina::Event::Mouse(m) => Self::Mouse(m),
            termina::Event::WindowResized(w) => Self::WindowResized(w),
            termina::Event::Paste(p) => Self::Paste(p),
            termina::Event::FocusIn => Self::FocusIn,
            termina::Event::FocusOut => Self::FocusOut,
            other => Self::Termina(other),
        }
    }
}
#[cfg(not(unix))]
use termina::{EventReader, PlatformWaker};
use termina::{PlatformTerminal, Terminal as _};

static TERM: Mutex<Option<PlatformTerminal>> = Mutex::new(None);
#[cfg(unix)]
static STDIN_IS_TTY: AtomicBool = AtomicBool::new(false);

fn term() -> MutexGuard<'static, Option<PlatformTerminal>> {
    TERM.lock().unwrap_or_else(|e| e.into_inner())
}

/// Open the terminal (once; stdin / stdout when they are terminals, else the controlling tty).
pub fn open() -> io::Result<()> {
    let mut t = term();
    if t.is_none() {
        *t = Some(PlatformTerminal::new()?);
        #[cfg(unix)]
        // SAFETY: isatty only inspects STDIN_FILENO.
        STDIN_IS_TTY.store(unsafe { libc::isatty(libc::STDIN_FILENO) } != 0, Ordering::Relaxed);
    }
    Ok(())
}

/// Raw mode (opens the terminal first if needed).
pub fn enter_raw() -> io::Result<()> {
    open()?;
    match term().as_mut() {
        Some(t) => t.enter_raw_mode(),
        None => Ok(()),
    }
}

/// Back to the modes the terminal had when it was opened (handoff, exit).
pub fn leave_raw() {
    if let Some(t) = term().as_mut() {
        let _ = t.enter_cooked_mode();
    }
}

/// Give the terminal up for good (exit): cooked mode, and on Windows the console modes and code
/// pages as they were. An [`Events`] still alive keeps reading from its own handle.
pub fn close() {
    drop(term().take());
    #[cfg(unix)]
    STDIN_IS_TTY.store(false, Ordering::Relaxed);
}

/// The window size including optional pixel dimensions (`ws_xpixel`, `ws_ypixel` on Unix).
/// While the TUI runs and `capture` redirects fd 1 to a pipe, the saved terminal fd is queried.
pub fn window_size() -> io::Result<WindowSize> {
    #[cfg(unix)]
    if let Some(fd) = crate::capture::tty_fd() {
        return fd_window_size(fd);
    }
    if let Some(t) = term().as_ref() {
        return t.get_dimensions();
    }
    #[cfg(windows)]
    {
        let (cols, rows) = console_size()?;
        Ok(WindowSize { cols, rows, pixel_width: None, pixel_height: None })
    }
    #[cfg(not(windows))]
    {
        PlatformTerminal::new()?.get_dimensions()
    }
}

/// The window size as (columns, rows).
pub fn size() -> io::Result<(u16, u16)> {
    let ws = window_size()?;
    Ok((ws.cols, ws.rows))
}

#[cfg(unix)]
fn fd_window_size(fd: i32) -> io::Result<WindowSize> {
    let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ writes one winsize through a valid pointer
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if ws.ws_col == 0 || ws.ws_row == 0 {
        return Err(io::Error::other("the terminal reports a zero size"));
    }
    Ok(WindowSize {
        cols: ws.ws_col,
        rows: ws.ws_row,
        pixel_width: (ws.ws_xpixel > 0).then_some(ws.ws_xpixel),
        pixel_height: (ws.ws_ypixel > 0).then_some(ws.ws_ypixel),
    })
}

/// termina's event source reads the new size from fd 1 on SIGWINCH; with fd 1 captured that
/// fails with ENOTTY although nothing is wrong with the input. That error is a resize.
#[cfg(not(unix))]
fn is_capture_resize(e: &io::Error) -> bool {
    let _ = e;
    false
}

/// A resize event with the size (and pixel dimensions when reported) asked from the terminal itself.
fn resize_event() -> Option<Event> {
    Some(Event::WindowResized(window_size().ok()?))
}

/// On Windows, `termina` turns `WINDOW_BUFFER_SIZE_EVENT` into `Event::WindowResized` using
/// `dwSize` (the console buffer dimensions, e.g. 9001 rows in classic `conhost.exe`) instead of
/// `srWindow` (the visible viewport). Prefer the queried viewport size when available.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn resolve_windows_resize(ev: WindowSize, viewport: Option<(u16, u16)>) -> WindowSize {
    match viewport {
        Some((cols, rows)) if cols > 0 && rows > 0 => WindowSize { cols, rows, ..ev },
        _ => ev,
    }
}

fn normalize_platform_event(ev: Event) -> Event {
    #[cfg(windows)]
    if let Event::WindowResized(ws) = ev {
        return Event::WindowResized(resolve_windows_resize(ws, size().ok()));
    }
    ev
}

#[cfg(windows)]
fn console_size_of(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<(u16, u16)> {
    use windows_sys::Win32::System::Console::{CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo};
    // SAFETY: the call fills a zeroed out-struct for the supplied console handle
    let info = unsafe {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(handle, &mut info) == 0 {
            return Err(io::Error::last_os_error());
        }
        info
    };
    let w = info.srWindow;
    let cols = (w.Right - w.Left + 1).max(0) as u16;
    let rows = (w.Bottom - w.Top + 1).max(0) as u16;
    if cols == 0 || rows == 0 {
        return Err(io::Error::other("the terminal reports a zero size"));
    }
    Ok((cols, rows))
}

#[cfg(windows)]
fn console_size() -> io::Result<(u16, u16)> {
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
    // SAFETY: querying the process stdout handle requires no external invariants
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    console_size_of(handle)
}

/// Retry wake interval while `Events::pause` waits for `mare-input` to leave its blocking poll.
const PAUSE_WAKE_INTERVAL: Duration = Duration::from_millis(200);

/// Terminal state notifications preserved across handoff (`Ctrl+G` / `Ctrl+Z`) while stale
/// pre-read key/paste/mouse events are discarded so an already-buffered Enter cannot submit
/// after an external editor exits.
pub fn is_preserved_across_handoff(ev: &Event) -> bool {
    matches!(ev, Event::FocusIn | Event::FocusOut | Event::WindowResized(_))
}

/// What the reader thread and the stream share.
struct Shared {
    /// the task to wake when input arrives (the newest poll's waker)
    waker: Mutex<Option<Waker>>,
    /// a wait on the terminal is in flight on the reader thread
    waiting: AtomicBool,
    /// events read and stamped with their arrival time by `mare-input`
    queue: Mutex<std::collections::VecDeque<io::Result<TimedEvent>>>,
    /// terminal handed off to an external program (Ctrl+G / Ctrl+Z): reader must not poll or read
    paused: AtomicBool,
    /// reset the byte decoder before the next read after a handoff pause
    clear_decoder: AtomicBool,
    /// true while `mare-input` is inside its blocking `poll`/`read` loop
    active_poll: Mutex<bool>,
    /// notified whenever `active_poll` transitions to `false`
    poll_idle: Condvar,
    /// the stream was dropped: the reader thread ends
    shutdown: AtomicBool,
}

struct ActivePollGuard<'a>(&'a Shared);

impl Drop for ActivePollGuard<'_> {
    fn drop(&mut self) {
        self.0.waiting.store(false, Ordering::SeqCst);
        let mut active = self.0.active_poll.lock().unwrap_or_else(|e| e.into_inner());
        *active = false;
        self.0.poll_idle.notify_all();
    }
}

impl Shared {
    fn wake(&self) {
        if let Some(w) = self.waker.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            w.wake_by_ref();
        }
    }

    fn push_batch(&self, batch: &mut Vec<io::Result<TimedEvent>>) {
        if batch.is_empty() {
            return;
        }
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).extend(batch.drain(..));
    }

    fn push(&self, item: io::Result<TimedEvent>) {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).push_back(item);
        self.wake();
    }
}

#[cfg(unix)]
struct ReaderWaker(Mutex<std::os::unix::net::UnixStream>);

#[cfg(unix)]
impl ReaderWaker {
    fn wake(&self) -> io::Result<()> {
        use std::io::Write as _;
        match self.0.lock().unwrap_or_else(|e| e.into_inner()).write(&[0]) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(unix)]
enum UnixReadFd {
    Stdin,
    Owned(std::fs::File),
}

#[cfg(unix)]
impl UnixReadFd {
    fn raw_fd(&self) -> libc::c_int {
        use std::os::fd::AsRawFd as _;
        match self {
            Self::Stdin => libc::STDIN_FILENO,
            Self::Owned(f) => f.as_raw_fd(),
        }
    }
}

#[cfg(unix)]
fn open_unix_tty_read() -> io::Result<UnixReadFd> {
    // SAFETY: isatty only inspects STDIN_FILENO.
    // Check `STDIN_IS_TTY` recorded at `tty::open()` as well: if the PTY master closed between
    // `tty::open()` and `tty::events()`, Linux `isatty(STDIN_FILENO)` returns 0 (`EIO`), and
    // returning `Stdin` ensures the reader immediately observes EOF/EIO instead of falling back to
    // `/dev/tty` (`ENXIO`) when running without a controlling terminal.
    if STDIN_IS_TTY.load(Ordering::Relaxed) || unsafe { libc::isatty(libc::STDIN_FILENO) } != 0 {
        return Ok(UnixReadFd::Stdin);
    }
    let file = std::fs::OpenOptions::new().read(true).open("/dev/tty")?;
    Ok(UnixReadFd::Owned(file))
}

#[cfg(unix)]
fn poll_unix_fds(tty_fd: libc::c_int, wake_fd: libc::c_int, timeout: Option<Duration>) -> io::Result<(bool, bool)> {
    #[cfg(not(target_os = "macos"))]
    {
        let mut fds = [
            libc::pollfd { fd: tty_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake_fd, events: libc::POLLIN, revents: 0 },
        ];
        let ms = match timeout {
            None => -1,
            Some(d) => d.as_millis().min(i32::MAX as u128) as libc::c_int,
        };
        // SAFETY: `fds` points to 2 valid `pollfd` entries.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let tty_ready = (fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0;
        let wake_ready = (fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0;
        Ok((tty_ready, wake_ready))
    }
    #[cfg(target_os = "macos")]
    {
        // macOS `poll(2)` does not support `/dev/tty`; use `select(2)` as termina does.
        let mut readfds: libc::fd_set = unsafe { std::mem::zeroed() };
        unsafe {
            libc::FD_ZERO(&mut readfds);
            libc::FD_SET(tty_fd, &mut readfds);
            libc::FD_SET(wake_fd, &mut readfds);
        }
        let mut tv = timeout.map(|d| libc::timeval {
            tv_sec: d.as_secs().min(i64::MAX as u64) as libc::time_t,
            tv_usec: d.subsec_micros() as libc::suseconds_t,
        });
        let tv_ptr = tv.as_mut().map_or(std::ptr::null_mut(), |t| t as *mut libc::timeval);
        let nfds = tty_fd.max(wake_fd) + 1;
        let rc = unsafe { libc::select(nfds, &mut readfds, std::ptr::null_mut(), std::ptr::null_mut(), tv_ptr) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let tty_ready = unsafe { libc::FD_ISSET(tty_fd, &readfds) };
        let wake_ready = unsafe { libc::FD_ISSET(wake_fd, &readfds) };
        Ok((tty_ready, wake_ready))
    }
}

#[cfg(unix)]
fn drain_wake_pipe(wake_read: &mut std::os::unix::net::UnixStream) {
    use std::io::Read as _;
    let mut scratch = [0u8; 64];
    loop {
        match wake_read.read(&mut scratch) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

#[cfg(unix)]
fn read_fd_once(fd: libc::c_int, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: `buf` is valid for `buf.len()` writable bytes and `fd` is open.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Terminal input events as an async stream: `None` never comes before an error does, and an
/// error (EOF / EIO) ends the stream. Each event carries its arrival timestamp recorded when read
/// by `mare-input` (which exclusively owns the terminal reader and only waits on it while polled,
/// so handoff keeps its keys and burst input never reorders across threads).
pub struct Events {
    #[cfg(unix)]
    wake_reader: ReaderWaker,
    #[cfg(not(unix))]
    wake_reader: PlatformWaker,
    #[cfg(unix)]
    sigwinch: Option<tokio::signal::unix::Signal>,
    shared: Arc<Shared>,
    wait: Option<mpsc::SyncSender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    done: bool,
}

/// The input stream of the open terminal.
pub fn events() -> io::Result<Events> {
    #[cfg(unix)]
    {
        if term().is_none() {
            return Err(io::Error::other("the terminal is not open"));
        }
        Events::from_unix_read_fd(open_unix_tty_read()?)
    }
    #[cfg(not(unix))]
    {
        let reader = match term().as_ref() {
            Some(t) => t.event_reader(),
            None => return Err(io::Error::other("the terminal is not open")),
        };
        Events::new(reader)
    }
}

impl Events {
    #[cfg(unix)]
    fn from_unix_read_fd(read_fd: UnixReadFd) -> io::Result<Self> {
        use std::os::fd::AsRawFd as _;
        let (mut wake_read, wake_write) = std::os::unix::net::UnixStream::pair()?;
        wake_read.set_nonblocking(true)?;
        wake_write.set_nonblocking(true)?;
        let wake_reader = ReaderWaker(Mutex::new(wake_write));
        let sigwinch = tokio::runtime::Handle::try_current()
            .ok()
            .and_then(|_| tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok());
        let shared = Arc::new(Shared {
            waker: Mutex::new(None),
            waiting: AtomicBool::new(false),
            queue: Mutex::new(std::collections::VecDeque::new()),
            paused: AtomicBool::new(false),
            clear_decoder: AtomicBool::new(false),
            active_poll: Mutex::new(false),
            poll_idle: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let (wait, requests) = mpsc::sync_channel::<()>(1);
        let s = shared.clone();
        let thread = std::thread::Builder::new().name("mare-input".into()).spawn(move || {
            let tty_fd = read_fd.raw_fd();
            let wake_fd = wake_read.as_raw_fd();
            let mut decoder = crate::live_input::LiveDecoder::default();
            let mut decoded = Vec::new();
            let mut buf = [0u8; 1024];
            let flush_pending_esc = |decoder: &mut crate::live_input::LiveDecoder,
                                     decoded: &mut Vec<Event>,
                                     batch: &mut Vec<io::Result<TimedEvent>>| {
                decoded.clear();
                if let Some(at) = decoder.flush_esc(decoded) {
                    for ev in decoded.drain(..) {
                        batch.push(Ok(TimedEvent { event: normalize_platform_event(ev), at }));
                    }
                }
            };
            let feed_read = |decoder: &mut crate::live_input::LiveDecoder,
                             bytes: &[u8],
                             more: bool,
                             at: std::time::Instant,
                             decoded: &mut Vec<Event>,
                             batch: &mut Vec<io::Result<TimedEvent>>| {
                decoded.clear();
                if let Some(esc_at) = decoder.flush_expired_esc(at, decoded) {
                    for ev in decoded.drain(..) {
                        batch.push(Ok(TimedEvent { event: normalize_platform_event(ev), at: esc_at }));
                    }
                }
                decoder.feed_at(bytes, more, at, decoded);
                for ev in decoded.drain(..) {
                    batch.push(Ok(TimedEvent { event: normalize_platform_event(ev), at }));
                }
            };
            let remaining_timeout = |deadline: Option<std::time::Instant>| {
                deadline.map(|deadline| {
                    let rem = deadline.saturating_duration_since(std::time::Instant::now());
                    // Round up sub-millisecond remainders so `poll(2)` does not wake early and spin.
                    Duration::from_millis(rem.as_nanos().div_ceil(1_000_000) as u64)
                })
            };
            let remaining_esc_timeout =
                |decoder: &crate::live_input::LiveDecoder| remaining_timeout(decoder.esc_deadline());
            while requests.recv().is_ok() {
                {
                    let mut active = s.active_poll.lock().unwrap_or_else(|e| e.into_inner());
                    if s.shutdown.load(Ordering::SeqCst) {
                        s.waiting.store(false, Ordering::SeqCst);
                        return;
                    }
                    if s.paused.load(Ordering::SeqCst) {
                        s.waiting.store(false, Ordering::SeqCst);
                        continue;
                    }
                    *active = true;
                }
                if s.clear_decoder.swap(false, Ordering::SeqCst) {
                    decoder.reset();
                }
                {
                    let _active_guard = ActivePollGuard(&s);
                    let mut batch = Vec::new();
                    loop {
                        if s.shutdown.load(Ordering::SeqCst) {
                            return;
                        }
                        if s.paused.load(Ordering::SeqCst) {
                            break;
                        }
                        let wait_deadline =
                            if batch.is_empty() { decoder.next_deadline() } else { decoder.esc_deadline() };
                        match poll_unix_fds(tty_fd, wake_fd, remaining_timeout(wait_deadline)) {
                            Ok((tty_ready, wake_ready)) => {
                                if wake_ready {
                                    drain_wake_pipe(&mut wake_read);
                                }
                                if s.shutdown.load(Ordering::SeqCst) {
                                    return;
                                }
                                if s.paused.load(Ordering::SeqCst) {
                                    break;
                                }
                                if !tty_ready {
                                    if !wake_ready {
                                        if decoder.esc_deadline().is_some() {
                                            flush_pending_esc(&mut decoder, &mut decoded, &mut batch);
                                        } else {
                                            decoded.clear();
                                            if let Some(at) =
                                                decoder.flush_expired_esc(std::time::Instant::now(), &mut decoded)
                                            {
                                                for ev in decoded.drain(..) {
                                                    batch.push(Ok(TimedEvent {
                                                        event: normalize_platform_event(ev),
                                                        at,
                                                    }));
                                                }
                                            }
                                        }
                                        if !batch.is_empty() {
                                            break;
                                        }
                                    }
                                    continue;
                                }
                                match read_fd_once(tty_fd, &mut buf) {
                                    Ok(0) => {
                                        flush_pending_esc(&mut decoder, &mut decoded, &mut batch);
                                        s.push_batch(&mut batch);
                                        s.push(Err(io::Error::new(
                                            io::ErrorKind::UnexpectedEof,
                                            "terminal input reached end-of-file",
                                        )));
                                        return;
                                    }
                                    Ok(n) => {
                                        let at = std::time::Instant::now();
                                        feed_read(
                                            &mut decoder,
                                            &buf[..n],
                                            n == buf.len(),
                                            at,
                                            &mut decoded,
                                            &mut batch,
                                        );
                                        if !batch.is_empty() && decoder.esc_deadline().is_none() {
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        flush_pending_esc(&mut decoder, &mut decoded, &mut batch);
                                        s.push_batch(&mut batch);
                                        s.push(Err(e));
                                        return;
                                    }
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                                if s.paused.load(Ordering::SeqCst) {
                                    break;
                                }
                            }
                            Err(e) => {
                                flush_pending_esc(&mut decoder, &mut decoded, &mut batch);
                                s.push_batch(&mut batch);
                                s.push(Err(e));
                                return;
                            }
                        }
                    }
                    if !batch.is_empty() {
                        // Drain any already-buffered bytes without blocking so burst input stays
                        // strictly FIFO on this thread and reaches the queue as one batch without
                        // exposing a half-drained prefix to `try_next`.
                        while !s.shutdown.load(Ordering::SeqCst) && !s.paused.load(Ordering::SeqCst) {
                            let drain_timeout = remaining_esc_timeout(&decoder).unwrap_or(Duration::ZERO);
                            match poll_unix_fds(tty_fd, wake_fd, Some(drain_timeout)) {
                                Ok((tty_ready, wake_ready)) => {
                                    if wake_ready {
                                        drain_wake_pipe(&mut wake_read);
                                    }
                                    if s.paused.load(Ordering::SeqCst) {
                                        break;
                                    }
                                    if !tty_ready {
                                        if !wake_ready {
                                            flush_pending_esc(&mut decoder, &mut decoded, &mut batch);
                                            break;
                                        }
                                        if decoder.esc_deadline().is_none() {
                                            break;
                                        }
                                        continue;
                                    }
                                    match read_fd_once(tty_fd, &mut buf) {
                                        Ok(0) => {
                                            flush_pending_esc(&mut decoder, &mut decoded, &mut batch);
                                            s.push_batch(&mut batch);
                                            s.push(Err(io::Error::new(
                                                io::ErrorKind::UnexpectedEof,
                                                "terminal input reached end-of-file",
                                            )));
                                            return;
                                        }
                                        Ok(n) => {
                                            let at = std::time::Instant::now();
                                            feed_read(
                                                &mut decoder,
                                                &buf[..n],
                                                n == buf.len(),
                                                at,
                                                &mut decoded,
                                                &mut batch,
                                            );
                                        }
                                        Err(e) => {
                                            flush_pending_esc(&mut decoder, &mut decoded, &mut batch);
                                            s.push_batch(&mut batch);
                                            s.push(Err(e));
                                            return;
                                        }
                                    }
                                }
                                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                                Err(e) => {
                                    flush_pending_esc(&mut decoder, &mut decoded, &mut batch);
                                    s.push_batch(&mut batch);
                                    s.push(Err(e));
                                    return;
                                }
                            }
                        }
                        s.push_batch(&mut batch);
                    }
                }
                if !s.paused.load(Ordering::SeqCst) {
                    s.wake();
                }
            }
            drop(read_fd);
        })?;
        Ok(Self { wake_reader, sigwinch, shared, wait: Some(wait), thread: Some(thread), done: false })
    }

    #[cfg(not(unix))]
    fn new(reader: EventReader) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            waker: Mutex::new(None),
            waiting: AtomicBool::new(false),
            queue: Mutex::new(std::collections::VecDeque::new()),
            paused: AtomicBool::new(false),
            clear_decoder: AtomicBool::new(false),
            active_poll: Mutex::new(false),
            poll_idle: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let (wait, requests) = mpsc::sync_channel::<()>(1);
        let wake_reader = reader.waker();
        let (r, s) = (reader, shared.clone());
        let thread = std::thread::Builder::new().name("mare-input".into()).spawn(move || {
            // Sole owner of `r`: block until input arrives, drain any already-buffered events in
            // strict FIFO order stamped with their arrival Instant, then return to `requests` so
            // handoff (Ctrl+Z / Ctrl+G) never has keys stolen while the loop isn't polling.
            while requests.recv().is_ok() {
                {
                    let mut active = s.active_poll.lock().unwrap_or_else(|e| e.into_inner());
                    if s.shutdown.load(Ordering::SeqCst) {
                        s.waiting.store(false, Ordering::SeqCst);
                        return;
                    }
                    if s.paused.load(Ordering::SeqCst) {
                        s.waiting.store(false, Ordering::SeqCst);
                        continue;
                    }
                    *active = true;
                }
                {
                    let _active_guard = ActivePollGuard(&s);
                    let mut batch = Vec::new();
                    loop {
                        if s.shutdown.load(Ordering::SeqCst) {
                            return;
                        }
                        if s.paused.load(Ordering::SeqCst) {
                            break;
                        }
                        match r.poll(None, |_| true) {
                            Ok(true) => {
                                if s.paused.load(Ordering::SeqCst) {
                                    break;
                                }
                                match r.read(|_| true) {
                                    Ok(ev) => {
                                        batch.push(Ok(TimedEvent {
                                            event: normalize_platform_event(Event::from(ev)),
                                            at: std::time::Instant::now(),
                                        }));
                                        break;
                                    }
                                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                                        if s.paused.load(Ordering::SeqCst) {
                                            break;
                                        }
                                    }
                                    Err(e) if is_capture_resize(&e) => {
                                        if let Some(ev) = resize_event() {
                                            batch.push(Ok(TimedEvent { event: ev, at: std::time::Instant::now() }));
                                        }
                                        break;
                                    }
                                    Err(e) => {
                                        s.push_batch(&mut batch);
                                        s.push(Err(e));
                                        return;
                                    }
                                }
                            }
                            Ok(false) => {
                                if s.shutdown.load(Ordering::SeqCst) {
                                    return;
                                }
                                if s.paused.load(Ordering::SeqCst) {
                                    break;
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                                if s.paused.load(Ordering::SeqCst) {
                                    break;
                                }
                            }
                            Err(e) if is_capture_resize(&e) => {
                                if let Some(ev) = resize_event() {
                                    batch.push(Ok(TimedEvent { event: ev, at: std::time::Instant::now() }));
                                }
                                break;
                            }
                            Err(e) => {
                                s.push_batch(&mut batch);
                                s.push(Err(e));
                                return;
                            }
                        }
                    }
                    if !batch.is_empty() {
                        // Drain any already-buffered events without blocking so burst input stays
                        // strictly FIFO on this thread and reaches the queue as one batch without
                        // exposing a half-drained prefix to `try_next`.
                        while !s.shutdown.load(Ordering::SeqCst) && !s.paused.load(Ordering::SeqCst) {
                            match r.poll(Some(Duration::ZERO), |_| true) {
                                Ok(true) => {
                                    if s.paused.load(Ordering::SeqCst) {
                                        break;
                                    }
                                    match r.read(|_| true) {
                                        Ok(ev) => batch.push(Ok(TimedEvent {
                                            event: normalize_platform_event(Event::from(ev)),
                                            at: std::time::Instant::now(),
                                        })),
                                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                                        Err(e) if is_capture_resize(&e) => {
                                            if let Some(ev) = resize_event() {
                                                batch.push(Ok(TimedEvent { event: ev, at: std::time::Instant::now() }));
                                            }
                                        }
                                        Err(e) => {
                                            s.push_batch(&mut batch);
                                            s.push(Err(e));
                                            return;
                                        }
                                    }
                                }
                                Ok(false) => break,
                                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                                Err(e) if is_capture_resize(&e) => {
                                    if let Some(ev) = resize_event() {
                                        batch.push(Ok(TimedEvent { event: ev, at: std::time::Instant::now() }));
                                    }
                                }
                                Err(e) => {
                                    s.push_batch(&mut batch);
                                    s.push(Err(e));
                                    return;
                                }
                            }
                        }
                        s.push_batch(&mut batch);
                    }
                }
                if !s.paused.load(Ordering::SeqCst) {
                    s.wake();
                }
            }
        })?;
        Ok(Self { wake_reader, shared, wait: Some(wait), thread: Some(thread), done: false })
    }

    /// Park `mare-input` without discarding already-queued events (used when transitioning out of
    /// the boot install screen so pre-read input can be drained before dropping the reader).
    pub fn pause_keep_queue(&mut self) {
        self.shared.paused.store(true, Ordering::SeqCst);
        let mut active = self.shared.active_poll.lock().unwrap_or_else(|e| e.into_inner());
        while *active {
            let _ = self.wake_reader.wake();
            let (guard, _) =
                self.shared.poll_idle.wait_timeout(active, PAUSE_WAKE_INTERVAL).unwrap_or_else(|e| e.into_inner());
            active = guard;
        }
    }

    /// Cancel any in-flight terminal poll, keep `mare-input` parked until [`Self::resume`], and
    /// discard stale pre-read key/paste/mouse events (retaining focus/resize state notifications).
    /// Call before handing the tty to an external program (Ctrl+G editor, Ctrl+Z) so `mare-input`
    /// cannot race the external program on stdin or replay pre-handoff keys on return.
    pub fn pause(&mut self) {
        self.pause_keep_queue();
        let mut q = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.retain(|item| match item {
            Ok(te) => is_preserved_across_handoff(&te.event),
            Err(_) => true,
        });
        self.shared.clear_decoder.store(true, Ordering::SeqCst);
    }

    /// Drain every already-queued event (including while paused) in FIFO order.
    pub fn drain_queued(&mut self) -> Vec<io::Result<TimedEvent>> {
        let mut q = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        let drained: Vec<_> = q.drain(..).collect();
        if drained.iter().any(Result::is_err) {
            self.done = true;
        }
        drained
    }

    /// Allow `mare-input` to poll the terminal again after reclaiming the tty.
    pub fn resume(&mut self) {
        self.shared.paused.store(false, Ordering::SeqCst);
    }

    /// Drain an already-queued event without waking `mare-input` or replacing the task waker.
    pub fn try_next(&mut self) -> Option<io::Result<TimedEvent>> {
        if self.done || self.shared.paused.load(Ordering::SeqCst) {
            return None;
        }
        let item = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front()?;
        if item.is_err() {
            self.done = true;
        }
        Some(item)
    }

    /// The next event (`Err`: the terminal is gone; `None` after that).
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<io::Result<TimedEvent>>> {
        if self.done {
            return Poll::Ready(None);
        }
        if self.shared.paused.load(Ordering::SeqCst) {
            return Poll::Pending;
        }
        {
            let mut w = self.shared.waker.lock().unwrap_or_else(|e| e.into_inner());
            if !w.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                *w = Some(cx.waker().clone());
            }
        }
        if let Some(item) = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front() {
            if item.is_err() {
                self.done = true;
            }
            return Poll::Ready(Some(item));
        }
        #[cfg(unix)]
        if let Some(sigwinch) = self.sigwinch.as_mut()
            && matches!(sigwinch.poll_recv(cx), Poll::Ready(Some(())))
        {
            while matches!(sigwinch.poll_recv(cx), Poll::Ready(Some(()))) {}
            if let Some(ev) = resize_event() {
                return Poll::Ready(Some(Ok(TimedEvent { event: ev, at: std::time::Instant::now() })));
            }
        }
        if !self.shared.waiting.swap(true, Ordering::SeqCst) {
            if let Some(item) = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front() {
                self.shared.waiting.store(false, Ordering::SeqCst);
                if item.is_err() {
                    self.done = true;
                }
                return Poll::Ready(Some(item));
            }
            if self.wait.as_ref().is_none_or(|wait| wait.try_send(()).is_err()) {
                self.shared.waiting.store(false, Ordering::SeqCst);
                if let Some(item) = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front() {
                    if item.is_err() {
                        self.done = true;
                    }
                    return Poll::Ready(Some(item));
                }
            }
        }
        Poll::Pending
    }

    /// The next event (`Err`: the terminal is gone; `None` after that).
    pub async fn next(&mut self) -> Option<io::Result<TimedEvent>> {
        std::future::poll_fn(|cx| self.poll_next(cx)).await
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        drop(self.wait.take());
        let _ = self.wake_reader.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_from_maps_all_termina_variants() {
        let resize = termina::WindowSize { cols: 80, rows: 24, pixel_width: None, pixel_height: None };
        assert!(matches!(Event::from(termina::Event::WindowResized(resize)), Event::WindowResized(_)));
        assert!(matches!(Event::from(termina::Event::Paste("abc".into())), Event::Paste(p) if p == "abc"));
        assert!(matches!(Event::from(termina::Event::FocusIn), Event::FocusIn));
        assert!(matches!(Event::from(termina::Event::FocusOut), Event::FocusOut));
        let mouse = termina::event::MouseEvent {
            kind: termina::event::MouseEventKind::Moved,
            column: 1,
            row: 2,
            modifiers: termina::event::Modifiers::NONE,
        };
        assert!(matches!(Event::from(termina::Event::Mouse(mouse)), Event::Mouse(_)));
    }

    #[cfg(unix)]
    #[test]
    fn fd_window_size_preserves_pixel_dimensions() {
        use std::os::fd::{AsRawFd, FromRawFd};
        let (mut master, mut slave) = (-1, -1);
        let mut ws = libc::winsize { ws_row: 24, ws_col: 80, ws_xpixel: 1280, ws_ypixel: 768 };
        // SAFETY: openpty initializes master/slave from the provided winsize.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::addr_of_mut!(ws) as _,
                )
            },
            0
        );
        let _master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let got = fd_window_size(slave.as_raw_fd()).unwrap();
        assert_eq!(got.cols, 80);
        assert_eq!(got.rows, 24);
        assert_eq!(got.pixel_width, Some(1280));
        assert_eq!(got.pixel_height, Some(768));
    }

    #[cfg(unix)]
    fn open_test_pty() -> (std::fs::File, std::fs::File) {
        use std::os::fd::FromRawFd;
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: openpty initializes both descriptors on success.
        assert_eq!(
            unsafe {
                libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
            },
            0
        );
        // Put the slave in raw mode so escape sequences and control bytes arrive untouched.
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut termios) }, 0);
        unsafe { libc::cfmakeraw(&mut termios) };
        assert_eq!(unsafe { libc::tcsetattr(slave, libc::TCSANOW, &termios) }, 0);
        // SAFETY: openpty returned two owned descriptors.
        unsafe { (std::fs::File::from_raw_fd(master), std::fs::File::from_raw_fd(slave)) }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_reader_preserves_csi_u_base_layout_key_over_pty() {
        use std::io::Write as _;
        let (mut master, slave) = open_test_pty();
        let mut events = Events::from_unix_read_fd(UnixReadFd::Owned(slave)).unwrap();
        // Raw Kitty CSI-u sequence for Cyrillic `Ctrl+с` (primary 1089, base layout 99 = 'c', Ctrl).
        master.write_all(b"\x1b[1089::99;5u").unwrap();
        master.flush().unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(3), events.next())
            .await
            .expect("timed out reading CSI-u from PTY")
            .expect("stream ended")
            .expect("read error");
        let Event::Key(k) = ev.event else { panic!("expected Key event, got {:?}", ev.event) };
        assert_eq!(k.code, KeyCode::Char('с'));
        assert_eq!(k.modifiers, Modifiers::CONTROL);
        assert_eq!(k.base_layout_key, Some('c'));
        assert!(crate::keys::is_ctrl_c(&k));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_reader_flushes_lone_esc_after_timeout_over_pty() {
        use std::io::Write as _;
        let (mut master, slave) = open_test_pty();
        let mut events = Events::from_unix_read_fd(UnixReadFd::Owned(slave)).unwrap();
        master.write_all(b"\x1b").unwrap();
        master.flush().unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(3), events.next())
            .await
            .expect("timed out waiting for lone ESC after timeout")
            .expect("stream ended")
            .expect("read error");
        assert!(matches!(ev.event, Event::Key(k) if k.code == KeyCode::Escape), "expected Esc, got {:?}", ev.event);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pause_discards_preread_keys_and_preserves_focus_state() {
        use std::io::Write as _;
        let (mut master, slave) = open_test_pty();
        let mut events = Events::from_unix_read_fd(UnixReadFd::Owned(slave)).unwrap();
        // Write Ctrl+G, Enter, and FocusIn in one burst so the reader buffers all three.
        master.write_all(b"\x07\r\x1b[I").unwrap();
        master.flush().unwrap();
        let first = tokio::time::timeout(Duration::from_secs(3), events.next())
            .await
            .expect("timed out reading first key")
            .expect("stream ended")
            .expect("read error");
        assert!(
            matches!(first.event, Event::Key(k) if k.code == KeyCode::Char('g') && k.modifiers == Modifiers::CONTROL)
        );
        // Pausing for handoff drops the pre-read Enter while keeping FocusIn.
        events.pause();
        events.resume();
        let next = tokio::time::timeout(Duration::from_secs(3), events.next())
            .await
            .expect("timed out reading retained FocusIn")
            .expect("stream ended")
            .expect("read error");
        assert_eq!(next.event, Event::FocusIn);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pause_keep_queue_and_drain_queued_preserve_fifo_input() {
        use std::io::Write as _;
        let (mut master, slave) = open_test_pty();
        let mut events = Events::from_unix_read_fd(UnixReadFd::Owned(slave)).unwrap();
        master.write_all(b"abcd").unwrap();
        master.flush().unwrap();
        let first = tokio::time::timeout(Duration::from_secs(3), events.next())
            .await
            .expect("timed out reading first char")
            .expect("stream ended")
            .expect("read error");
        assert!(matches!(first.event, Event::Key(k) if k.code == KeyCode::Char('a')));
        events.pause_keep_queue();
        let rest: Vec<char> = events
            .drain_queued()
            .into_iter()
            .filter_map(|r| match r.ok()?.event {
                Event::Key(k) => match k.code {
                    KeyCode::Char(c) => Some(c),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(rest, vec!['b', 'c', 'd']);
    }

    #[test]
    fn windows_resize_prefers_viewport_over_buffer_size() {
        let buf = WindowSize { cols: 120, rows: 9001, pixel_width: Some(960), pixel_height: Some(540) };
        let resolved = resolve_windows_resize(buf, Some((120, 30)));
        assert_eq!(resolved.cols, 120);
        assert_eq!(resolved.rows, 30);
        assert_eq!(resolved.pixel_width, Some(960));
        assert_eq!(resolved.pixel_height, Some(540));

        // Falls back to the event dimensions when viewport query is unavailable or zero.
        assert_eq!(resolve_windows_resize(buf, None), buf);
        assert_eq!(resolve_windows_resize(buf, Some((0, 30))), buf);
        assert_eq!(resolve_windows_resize(buf, Some((120, 0))), buf);
    }

    #[cfg(windows)]
    #[test]
    fn windows_console_size_reads_viewport_not_buffer_height() {
        use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        use windows_sys::Win32::System::Console::{
            CONSOLE_SCREEN_BUFFER_INFO, CONSOLE_TEXTMODE_BUFFER, COORD, CreateConsoleScreenBuffer,
            GetConsoleScreenBufferInfo, SetConsoleScreenBufferSize,
        };
        // SAFETY: creating an isolated offscreen console screen buffer does not change the active
        // console buffer; handle is closed before returning.
        unsafe {
            let handle = CreateConsoleScreenBuffer(
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                CONSOLE_TEXTMODE_BUFFER,
                std::ptr::null(),
            );
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                // Skip when the test runner has no attached Windows console at all.
                return;
            }
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            let got = GetConsoleScreenBufferInfo(handle, &mut info);
            if got != 0 {
                let win_cols = (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u16;
                let win_rows = (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u16;
                let tall_rows = win_rows.saturating_add(200);
                let _ = SetConsoleScreenBufferSize(handle, COORD { X: win_cols as i16, Y: tall_rows as i16 });
                let mut after: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
                assert_ne!(GetConsoleScreenBufferInfo(handle, &mut after), 0);
                let viewport = console_size_of(handle).expect("console_size_of offscreen buffer");
                let expected_cols = (after.srWindow.Right - after.srWindow.Left + 1) as u16;
                let expected_rows = (after.srWindow.Bottom - after.srWindow.Top + 1) as u16;
                assert_eq!(viewport, (expected_cols, expected_rows));
                let buf_event = WindowSize {
                    cols: after.dwSize.X as u16,
                    rows: after.dwSize.Y as u16,
                    pixel_width: None,
                    pixel_height: None,
                };
                assert!(
                    buf_event.rows > viewport.1,
                    "expected buffer height {} > viewport {}",
                    buf_event.rows,
                    viewport.1
                );
                let normalized = resolve_windows_resize(buf_event, Some(viewport));
                assert_eq!((normalized.cols, normalized.rows), viewport);
            }
            CloseHandle(handle);
        }
    }
}
