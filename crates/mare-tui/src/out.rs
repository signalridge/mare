//! The single write exit to the terminal: every byte mare writes to the tty goes
//! through here, in order, on one writer thread.
//!
//! - **the final check runs here, on every write** ([`guard`]): frames ([`write`]) may carry only
//!   the sequence classes the renderer emits (SGR, cursor movement, erase, scroll region, sync /
//!   cursor / paste modes, save/restore cursor, OSC 8 links to http/https); mare's own control
//!   writes ([`write_own`]: modes, the startup probe, title, notifications, OSC 52) additionally
//!   their OSC / DCS / query forms, with payloads checked as untrusted (no control bytes).
//!   Anything else — a stray control byte, an OSC 52 or title hidden in a frame, an unknown
//!   sequence — is dropped and counted ([`dropped`]), never written;
//! - title (OSC 2), desktop notifications (OSC 9 / 777 / 99) and clipboard (OSC 52) are built
//!   only by the functions below, which clean their content (labels: no escapes, controls,
//!   bidi), cap OSC 52 at 100 KB and add tmux / screen passthrough;
//! - the writer thread keeps the event loop off blocking tty writes and gives backpressure: the
//!   loop draws the next frame only when the previous one is out ([`busy`], [`drained`]), so a
//!   slow terminal gets fewer, larger frames instead of an ever-growing queue (OpenTUI renderer).

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Max OSC 52 payload (before base64)`.
pub const OSC52_MAX: usize = 100 * 1024;

/// Who built the bytes: the renderer (frames) or mare's own control writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Frame,
    Own,
}

enum Msg {
    Bytes(Vec<u8>, Origin),
}

struct Exit {
    tx: Mutex<Sender<Msg>>,
    pending: AtomicUsize,
    notify: tokio::sync::Notify,
}

static EXIT: OnceLock<Exit> = OnceLock::new();

/// Test / headless seam: when set, bytes are captured here instead of written to stdout.
static CAPTURE: Mutex<Option<Vec<u8>>> = Mutex::new(None);

fn exit() -> &'static Exit {
    EXIT.get_or_init(|| {
        let (tx, rx) = channel::<Msg>();
        std::thread::Builder::new()
            .name("mare-tty-writer".into())
            .spawn(move || {
                let mut stdout = std::io::stdout();
                while let Ok(Msg::Bytes(b, origin)) = rx.recv() {
                    let (b, n) = guard(&b, origin);
                    note_dropped(n);
                    let captured = {
                        let mut c = CAPTURE.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(buf) = c.as_mut() {
                            buf.extend_from_slice(&b);
                            true
                        } else {
                            false
                        }
                    };
                    if !captured {
                        // while external output is captured, fd 1 is a pipe: write to the
                        // saved terminal fd instead (crate::capture)
                        match crate::capture::tty_fd() {
                            Some(fd) => write_fd(fd, &b),
                            None => {
                                write_frame(|chunk| stdout.write(chunk), &b);
                                let _ = stdout.flush();
                            }
                        }
                    }
                    let e = EXIT.get().expect("writer started after init");
                    if e.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
                        e.notify.notify_waiters();
                    }
                }
            })
            .expect("spawn tty writer");
        Exit { tx: Mutex::new(tx), pending: AtomicUsize::new(0), notify: tokio::sync::Notify::new() }
    })
}

/// Write all of `b` through `write_some`, so a frame never stops halfway with synchronized
/// output still on (`?2026h` written, its `?2026l` not — the terminal would hold every later
/// frame until its own timeout): EINTR retries, EAGAIN (a child left the shared tty
/// non-blocking) waits briefly and retries, and on any other failure — or if it stays blocked —
/// the closing `?2026l` is still attempted once.
fn write_frame(mut write_some: impl FnMut(&[u8]) -> std::io::Result<usize>, b: &[u8]) {
    const END_SYNC: &[u8] = b"\x1b[?2026l";
    let mut rest = b;
    let mut waited = std::time::Duration::ZERO;
    while !rest.is_empty() {
        match write_some(rest) {
            Ok(0) => break,
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && waited < std::time::Duration::from_secs(2) => {
                std::thread::sleep(std::time::Duration::from_millis(2));
                waited += std::time::Duration::from_millis(2);
            }
            Err(_) => break,
        }
    }
    let opened = b.windows(8).any(|w| w == b"\x1b[?2026h");
    if !rest.is_empty() && opened && rest.windows(END_SYNC.len()).any(|w| w == END_SYNC) {
        let mut end = END_SYNC;
        for _ in 0..3 {
            match write_some(end) {
                Ok(n) if n >= end.len() => break,
                Ok(n) => end = &end[n..],
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
            }
        }
    }
}

/// Write all of `b` to a raw fd (the saved terminal while output is captured; Unix only —
/// [`crate::capture::tty_fd`] is always `None` elsewhere).
#[cfg(unix)]
fn write_fd(fd: i32, b: &[u8]) {
    write_frame(
        |chunk| {
            // SAFETY: `fd` is the dup of the terminal owned by crate::capture
            let n = unsafe { libc::write(fd, chunk.as_ptr().cast(), chunk.len()) };
            if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
        },
        b,
    );
}

#[cfg(not(unix))]
fn write_fd(_fd: i32, _b: &[u8]) {}

/// Queue a rendered frame for the terminal (in order). Empty input writes nothing.
pub fn write(bytes: Vec<u8>) {
    send(bytes, Origin::Frame);
}

/// Queue one of mare's own control writes (modes, the probe; title / notify / OSC 52 use this).
pub fn write_own(bytes: Vec<u8>) {
    send(bytes, Origin::Own);
}

/// Temporarily puts a Unix fd into `O_NONBLOCK` mode and restores its original flags on drop so
/// `libc::write` can never block past the caller's deadline even if another thread fills the
/// tty/pipe buffer between `poll` and `write`.
#[cfg(unix)]
struct NonblockGuard {
    fd: libc::c_int,
    orig_flags: libc::c_int,
    changed: bool,
}

#[cfg(unix)]
impl NonblockGuard {
    fn enter(fd: libc::c_int) -> Option<Self> {
        // SAFETY: `F_GETFL` takes no extra arguments and returns the current file status flags.
        let orig_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if orig_flags < 0 {
            return None;
        }
        let changed = (orig_flags & libc::O_NONBLOCK) == 0;
        if changed {
            // SAFETY: setting `O_NONBLOCK` on the fd ensures writes return `EAGAIN` instead of
            // blocking unboundedly.
            if unsafe { libc::fcntl(fd, libc::F_SETFL, orig_flags | libc::O_NONBLOCK) } < 0 {
                return None;
            }
        }
        Some(Self { fd, orig_flags, changed })
    }
}

#[cfg(unix)]
impl Drop for NonblockGuard {
    fn drop(&mut self) {
        if self.changed {
            // SAFETY: restores the original file status flags captured in `enter`.
            let _ = unsafe { libc::fcntl(self.fd, libc::F_SETFL, self.orig_flags) };
        }
    }
}

#[cfg(unix)]
fn write_fd_bounded(fd: libc::c_int, bytes: &[u8], timeout: Duration) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if timeout.is_zero() {
        return false;
    }
    let Some(_nonblock) = NonblockGuard::enter(fd) else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    let mut rest = bytes;
    while !rest.is_empty() {
        if deadline.saturating_duration_since(Instant::now()).is_zero() {
            return false;
        }
        // SAFETY: `rest` is valid for `rest.len()` bytes and `fd` is currently `O_NONBLOCK`.
        let written = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
        if written > 0 {
            rest = &rest[written as usize..];
            continue;
        }
        if written == 0 {
            return false;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if err.kind() != std::io::ErrorKind::WouldBlock {
            return false;
        }
        loop {
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                return false;
            }
            let ms = rem.as_millis().min(i32::MAX as u128).max(1) as libc::c_int;
            let mut pfd = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
            // SAFETY: `pfd` points to one valid `pollfd` entry.
            let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
            if rc < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return false;
            }
            if rc == 0 || (pfd.revents & libc::POLLOUT) == 0 {
                return false;
            }
            break;
        }
    }
    true
}

/// Write `bytes` directly to the terminal fd with a short bounded wait, bypassing the async
/// writer thread (used by watchdog force-exit cleanup so a wedged writer or full pipe cannot
/// block forced exit). Returns `true` when every byte was written.
pub fn write_direct_bounded(bytes: &[u8], timeout: Duration) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let (bytes, n) = guard(bytes, Origin::Own);
    note_dropped(n);
    if let Ok(mut c) = CAPTURE.try_lock()
        && let Some(buf) = c.as_mut()
    {
        buf.extend_from_slice(&bytes);
        return true;
    }
    #[cfg(unix)]
    {
        let fd = crate::capture::tty_fd().unwrap_or(libc::STDOUT_FILENO);
        write_fd_bounded(fd, &bytes, timeout)
    }
    #[cfg(not(unix))]
    {
        if timeout.is_zero() {
            return false;
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<bool>(1);
        let spawned = std::thread::Builder::new().name("mare-force-leave".into()).spawn(move || {
            let mut stdout = std::io::stdout();
            let ok = stdout.write_all(&bytes).is_ok() && stdout.flush().is_ok();
            let _ = tx.send(ok);
        });
        if spawned.is_err() {
            return false;
        }
        rx.recv_timeout(timeout).unwrap_or(false)
    }
}

fn send(bytes: Vec<u8>, origin: Origin) {
    if bytes.is_empty() {
        return;
    }
    let e = exit();
    e.pending.fetch_add(1, Ordering::AcqRel);
    if e.tx.lock().unwrap_or_else(|p| p.into_inner()).send(Msg::Bytes(bytes, origin)).is_err() {
        e.pending.fetch_sub(1, Ordering::AcqRel);
    }
}

// ---- image sequences ----------------------------------------------------------------------------

/// Hashes of the image sequences core built: the final check
/// passes a Kitty APC `_G`, an iTerm2 OSC 1337 or a sixel DCS only when its exact bytes are here,
/// so the same kinds from model, tool or file text stay filtered. Bounded (oldest dropped).
type ImageSet = (std::collections::HashSet<u64>, std::collections::VecDeque<u64>);
static IMAGES: std::sync::LazyLock<Mutex<ImageSet>> = std::sync::LazyLock::new(|| Mutex::new(Default::default()));
const IMAGES_MAX: usize = 4096;

fn seq_hash(b: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

/// Register one complete image sequence built by core.
pub fn allow_image(seq: &[u8]) {
    let h = seq_hash(seq);
    let mut g = IMAGES.lock().unwrap_or_else(|e| e.into_inner());
    if g.0.insert(h) {
        g.1.push_back(h);
        while g.1.len() > IMAGES_MAX {
            if let Some(old) = g.1.pop_front() {
                g.0.remove(&old);
            }
        }
    }
}

/// Whether these exact bytes are a registered image sequence.
pub fn image_allowed(seq: &[u8]) -> bool {
    IMAGES.lock().unwrap_or_else(|e| e.into_inner()).0.contains(&seq_hash(seq))
}

/// End (exclusive) of the string sequence starting at `i` (APC / DCS: ST; OSC: BEL or ST).
fn string_seq_end(b: &[u8], i: usize, osc: bool) -> Option<usize> {
    let mut j = i + 2;
    while j < b.len() {
        if osc && b[j] == 0x07 {
            return Some(j + 1);
        }
        if b[j] == 0x1b && b.get(j + 1) == Some(&b'\\') {
            return Some(j + 2);
        }
        j += 1;
    }
    None
}

// ---- the final check ---------------------------------------------------------------------------

static DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn note_dropped(n: usize) {
    if n == 0 {
        return;
    }
    let total = DROPPED.fetch_add(n as u64, Ordering::Relaxed);
    // log the first occurrence and then every 1000th, never per frame
    if total == 0 || total / 1000 != (total + n as u64) / 1000 {
        tracing::warn!("write exit dropped {n} disallowed control byte(s)/sequence(s) (total {})", total + n as u64);
    }
}

/// Control bytes / sequences the final check dropped so far (`/profile`, tests).
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// CSI final bytes the renderer uses: SGR, cursor position/movement, erase, scroll, insert /
/// delete, scroll region, set/reset mode (2026 sync, ?25 cursor, …).
const FRAME_CSI: &[u8] = b"mHfABCDEFGJKSTLMPX@rhl";
/// Extra CSI finals for mare's own writes: kitty push/pop/query (u), DECRQM ($p), CPR (n), DA (c).
const OWN_CSI: &[u8] = b"upnc";
/// OSC numbers mare writes itself: title, link, notifications, OSC 11 query, clipboard, kitty.
const OWN_OSC: &[u32] = &[2, 8, 9, 11, 52, 99, 777];

fn is_ctl(c: char) -> bool {
    c.is_control()
}

/// The final check: returns the bytes that may reach the terminal and how many
/// disallowed items were dropped. Pure; see the module docs for what each origin may carry.
pub fn guard(b: &[u8], origin: Origin) -> (Vec<u8>, usize) {
    let own = origin == Origin::Own;
    let mut out = Vec::with_capacity(b.len());
    let mut dropped = 0usize;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == 0x1b {
            let Some(&k) = b.get(i + 1) else {
                dropped += 1;
                break;
            };
            // a registered image sequence (core's image primitive) passes as it is
            if matches!(k, b'_' | b'P' | b']')
                && let Some(end) = string_seq_end(b, i, k == b']')
                && image_allowed(&b[i..end])
            {
                out.extend_from_slice(&b[i..end]);
                i = end;
                continue;
            }
            match k {
                b'[' => {
                    let mut j = i + 2;
                    while j < b.len() && (0x30..=0x3f).contains(&b[j]) {
                        j += 1;
                    }
                    while j < b.len() && (0x20..=0x2f).contains(&b[j]) {
                        j += 1;
                    }
                    let Some(&f) = b.get(j) else {
                        dropped += 1;
                        break;
                    };
                    // The image-protocol/cell-size probe needs these exact queries. Keep
                    // other `q`/`t` forms (cursor style/window operations) out of both origins.
                    let parameters = &b[i + 2..j];
                    let own_probe = own && ((f == b'q' && parameters == b">") || (f == b't' && parameters == b"16"));
                    let ok = (0x40..=0x7e).contains(&f)
                        && (FRAME_CSI.contains(&f) || (own && OWN_CSI.contains(&f)) || own_probe);
                    if ok {
                        out.extend_from_slice(&b[i..=j]);
                    } else {
                        dropped += 1;
                    }
                    i = j + 1;
                }
                b']' => {
                    // OSC: payload up to BEL or ST
                    let mut j = i + 2;
                    let mut end = None;
                    while j < b.len() {
                        if b[j] == 0x07 {
                            end = Some((j, j + 1));
                            break;
                        }
                        if b[j] == 0x1b && b.get(j + 1) == Some(&b'\\') {
                            end = Some((j, j + 2));
                            break;
                        }
                        j += 1;
                    }
                    let Some((pend, next)) = end else {
                        dropped += 1;
                        break;
                    };
                    let body = String::from_utf8_lossy(&b[i + 2..pend]);
                    let num: Option<u32> = body.split(';').next().and_then(|n| n.parse().ok());
                    let clean = !body.chars().any(is_ctl);
                    let ok = clean
                        && match num {
                            Some(8) => {
                                // OSC 8 ; params ; uri — empty uri closes the link
                                let uri = body.splitn(3, ';').nth(2).unwrap_or("");
                                uri.is_empty() || uri.starts_with("http://") || uri.starts_with("https://")
                            }
                            Some(n) => own && OWN_OSC.contains(&n),
                            None => false,
                        };
                    if ok {
                        out.extend_from_slice(&b[i..next]);
                    } else {
                        dropped += 1;
                    }
                    i = next;
                }
                b'P' if own => {
                    // DCS (own writes only): tmux / screen passthrough of an own sequence, or an
                    // XTGETTCAP query (`+q` hex)
                    let mut j = i + 2;
                    while j < b.len() {
                        // inside tmux passthrough ESCs are doubled: skip the pairs
                        if b[j] == 0x1b && b.get(j + 1) == Some(&0x1b) {
                            j += 2;
                        } else if b[j] == 0x1b && b.get(j + 1) == Some(&b'\\') {
                            break;
                        } else {
                            j += 1;
                        }
                    }
                    if j >= b.len() {
                        dropped += 1;
                        break;
                    }
                    let body = &b[i + 2..j];
                    let mut end = j + 2;
                    let ok = if let Some(inner) = body.strip_prefix(b"tmux;") {
                        let un = String::from_utf8_lossy(inner).replace("\x1b\x1b", "\x1b");
                        guard(un.as_bytes(), Origin::Own).1 == 0
                    } else if let Some(hex) = body.strip_prefix(b"+q") {
                        hex.iter().all(|c| c.is_ascii_hexdigit() || *c == b';')
                    } else if body.first() == Some(&0x1b) {
                        // GNU screen passthrough may chunk an own escape sequence across `<= 252`-byte DCS blocks.
                        let mut reassembled = body.to_vec();
                        while b[end..].starts_with(b"\x1bP") {
                            let mut nj = end + 2;
                            while nj < b.len() && !(b[nj] == 0x1b && b.get(nj + 1) == Some(&b'\\')) {
                                nj += 1;
                            }
                            if nj >= b.len() {
                                break;
                            }
                            let chunk = &b[end + 2..nj];
                            if chunk.starts_with(b"tmux;") || chunk.starts_with(b"+q") || chunk.len() > 252 {
                                break;
                            }
                            reassembled.extend_from_slice(chunk);
                            end = nj + 2;
                        }
                        guard(&reassembled, Origin::Own).1 == 0
                    } else {
                        false
                    };
                    if ok {
                        out.extend_from_slice(&b[i..end]);
                    } else {
                        dropped += 1;
                    }
                    i = end;
                }
                b'7' | b'8' => {
                    out.extend_from_slice(&b[i..i + 2]);
                    i += 2;
                }
                _ => {
                    // any other ESC sequence (DCS / APC / PM / SOS in frames, two-byte escapes):
                    // drop it up to its terminator
                    dropped += 1;
                    let mut j = i + 2;
                    if matches!(k, b'P' | b'_' | b'^' | b'X') {
                        while j < b.len() && !(b[j] == 0x1b && b.get(j + 1) == Some(&b'\\')) {
                            j += 1;
                        }
                        j = (j + 2).min(b.len());
                    }
                    i = j;
                }
            }
            continue;
        }
        if c < 0x20 {
            // a bare BEL is allowed only in mare's own writes (the bell notification: protocol
            // "bel", or `bell` after an OSC notification); text and frames never ring
            if matches!(c, b'\r' | b'\n' | b'\t') || (own && c == 0x07) {
                out.push(c);
            } else {
                dropped += 1;
            }
            i += 1;
            continue;
        }
        if c == 0x7f {
            dropped += 1;
            i += 1;
            continue;
        }
        // C1 controls encoded as UTF-8 (U+0080..U+009F)
        if c == 0xc2 && b.get(i + 1).is_some_and(|n| (0x80..=0x9f).contains(n)) {
            dropped += 1;
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    (out, dropped)
}

/// A previous write is still in flight (the terminal is slow to drain): don't draw yet.
pub fn busy() -> bool {
    EXIT.get().is_some_and(|e| e.pending.load(Ordering::Acquire) > 0)
}

/// Resolves when the queue is empty (backpressure wakeup for the event loop).
pub async fn drained() {
    let e = exit();
    loop {
        let n = e.notify.notified();
        if e.pending.load(Ordering::Acquire) == 0 {
            return;
        }
        n.await;
    }
}

/// Block until everything queued has been written, or `timeout` passes. Used before code that
/// writes to the tty outside this exit (raw / cooked mode switches on enter/leave, suspend, exit).
pub fn wait_idle(timeout: Duration) {
    let Some(e) = EXIT.get() else { return };
    let end = Instant::now() + timeout;
    while e.pending.load(Ordering::Acquire) > 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_micros(200));
    }
}

/// Tests that capture the exit's output or flip global terminal state hold this so global render state is serialized in tests.
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Capture output instead of writing it (tests). Returns what was captured since the last call.
pub fn capture(on: bool) -> Vec<u8> {
    wait_idle(Duration::from_secs(2));
    let mut c = CAPTURE.lock().unwrap_or_else(|e| e.into_inner());
    let prev = c.take().unwrap_or_default();
    if on {
        *c = Some(vec![]);
    }
    prev
}

// ---- mare's own control sequences -------------------------------------------------------------

/// Window title (OSC 2) from a cleaned label.
pub fn title(text: &str) {
    let t = mare_shared::ansi::label(text, 120);
    write_own(format!("\x1b]2;{t}\x07").into_bytes());
}

/// Desktop notification (OSC 9 / 777 / 99 per `protocol`), followed by a BEL only when `bell`
/// is on; `protocol = "bel"` rings the bell alone. OSC 9 already ends in BEL on most terminals'
/// parsers, so an unconditional extra one rang twice.
pub fn notify(title: &str, body: &str, protocol: &str, bell: bool) {
    write_own(notify_bytes(title, body, protocol, bell));
}

/// The bytes [`notify`] writes.
pub fn notify_bytes(title: &str, body: &str, protocol: &str, bell: bool) -> Vec<u8> {
    if protocol == "bel" {
        return b"\x07".to_vec();
    }
    let seq = notification_sequence(title, body, protocol);
    if bell { format!("{seq}\x07") } else { seq }.into_bytes()
}

/// Copy to the system clipboard via OSC 52 — only on an explicit user copy.
/// Distinguishes [`Osc52Status::Written`], [`Osc52Status::TooLarge`], and [`Osc52Status::HandedOff`]
/// so callers never report a terminal clipboard write when the terminal was handed off.
pub fn osc52(text: &str) -> mare_shared::ui::Osc52Status {
    if text.len() > OSC52_MAX {
        return mare_shared::ui::Osc52Status::TooLarge;
    }
    if crate::modes::handed_off() {
        return mare_shared::ui::Osc52Status::HandedOff;
    }
    let b64 = base64_encode(text.as_bytes());
    write_own(passthrough(&format!("\x1b]52;c;{b64}\x07")).into_bytes());
    mare_shared::ui::Osc52Status::Written
}

/// Desktop-notification escape sequence: OSC 9 (iTerm2, WezTerm, Ghostty, …),
/// OSC 777 (urxvt, foot, VTE terminals), kitty's OSC 99. `protocol` = "auto" | "osc9" |
/// "osc777" | "osc99". Inside tmux / GNU screen the sequence is wrapped in a DCS passthrough.
pub fn notification_sequence(title: &str, body: &str, protocol: &str) -> String {
    let clean = |s: &str| mare_shared::ansi::label(s, 200).replace(';', ",");
    let (title, body) = (clean(title), clean(body));
    let proto = match protocol {
        "auto" | "" => detect_notification_protocol(),
        p => p,
    };
    let seq = match proto {
        "osc777" => format!("\x1b]777;notify;{title};{body}\x1b\\"),
        "osc99" => format!("\x1b]99;i=1:d=0;{title}\x1b\\\x1b]99;i=1:d=1:p=body;{body}\x1b\\"),
        _ => format!("\x1b]9;{title}: {body}\x07"),
    };
    passthrough(&seq)
}

fn detect_notification_protocol() -> &'static str {
    let term = std::env::var("TERM").unwrap_or_default();
    if std::env::var_os("KITTY_WINDOW_ID").is_some() || term.contains("kitty") {
        "osc99"
    } else if std::env::var_os("VTE_VERSION").is_some() || term.contains("rxvt") || term.starts_with("foot") {
        "osc777"
    } else {
        "osc9"
    }
}

const SCREEN_DCS_CHUNK: usize = 252;

/// Wrap `seq` for GNU `screen` DCS passthrough, replacing inner ST (`ESC \`) with BEL and
/// splitting into `<= 252`-byte UTF-8-aligned DCS chunks.
pub(crate) fn screen_passthrough(seq: &str) -> String {
    if seq.is_empty() {
        return String::new();
    }
    let sanitized = seq.replace("\x1b\\", "\x07");
    let mut out = String::new();
    let mut start = 0;
    while start < sanitized.len() {
        let mut end = (start + SCREEN_DCS_CHUNK).min(sanitized.len());
        while end > start && !sanitized.is_char_boundary(end) {
            end -= 1;
        }
        out.push_str("\x1bP");
        out.push_str(&sanitized[start..end]);
        out.push_str("\x1b\\");
        start = end;
    }
    out
}

/// Wrap an escape sequence so tmux / screen forward it to the outer terminal.
pub fn passthrough(seq: &str) -> String {
    if std::env::var_os("TMUX").is_some() {
        // tmux: DCS tmux; <seq with every ESC doubled> ST (needs `allow-passthrough on`)
        format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
    } else if std::env::var("TERM").is_ok_and(|t| t.starts_with("screen")) {
        screen_passthrough(seq)
    } else {
        seq.to_string()
    }
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_write_never_leaves_synchronized_output_on() {
        use std::io::{Error, ErrorKind};
        let frame = b"\x1b[?2026h0123456789abcdef\x1b[?2026l".to_vec();
        // EAGAIN twice, then the rest goes through: the whole frame arrives
        let mut out = Vec::new();
        let mut fails = 2;
        write_frame(
            |c| {
                if fails > 0 {
                    fails -= 1;
                    return Err(Error::from(ErrorKind::WouldBlock));
                }
                let n = c.len().min(5);
                out.extend_from_slice(&c[..n]);
                Ok(n)
            },
            &frame,
        );
        assert_eq!(out, frame);
        // a hard error halfway: the closing ?2026l is still written
        let mut out = Vec::new();
        let mut calls = 0;
        write_frame(
            |c| {
                calls += 1;
                if calls == 2 {
                    return Err(Error::from(ErrorKind::BrokenPipe));
                }
                let n = c.len().min(10);
                out.extend_from_slice(&c[..n]);
                Ok(n)
            },
            &frame,
        );
        assert!(out.starts_with(b"\x1b[?2026h"));
        assert!(out.ends_with(b"\x1b[?2026l"), "{:?}", String::from_utf8_lossy(&out));
        // EINTR is retried transparently
        let mut out = Vec::new();
        let mut intr = true;
        write_frame(
            |c| {
                if std::mem::take(&mut intr) {
                    return Err(Error::from(ErrorKind::Interrupted));
                }
                out.extend_from_slice(c);
                Ok(c.len())
            },
            &frame,
        );
        assert_eq!(out, frame);
    }

    fn g(b: &str, o: Origin) -> (String, usize) {
        let (out, n) = guard(b.as_bytes(), o);
        (String::from_utf8(out).unwrap(), n)
    }

    #[test]
    fn frames_keep_render_sequences_and_drop_everything_else() {
        let frame = "\x1b[?2026h\x1b[1;8r\x1b7\x1b[8;1H\x1b[31mhi\x1b[0m\x1b[K\x1b8\x1b]8;;https://x.test\x1b\\l\x1b]8;;\x1b\\\r\n\x1b[?2026l";
        assert_eq!(g(frame, Origin::Frame), (frame.to_string(), 0));
        // OSC 52, a title, a non-http link, DCS, a bare BEL, C1 and DEL never leave a frame
        let (out, n) = g(
            "a\x1b]52;c;ZXZpbA==\x07b\x1b]2;pwned\x07c\x1b]8;;javascript:x\x1b\\d\x1bPq\x1b\\e\x07f\u{9b}2Jg\x7f",
            Origin::Frame,
        );
        // the C1 CSI introducer is dropped, its parameters stay plain text
        assert_eq!((out.as_str(), n), ("abcdef2Jg", 7));
    }

    #[test]
    fn own_writes_allow_their_sequences_with_clean_payloads_only() {
        for ok in [
            "\x1b]2;mare\x07",
            "\x1b[>1u\x1b[?u\x1b[6n\x1b[c\x1b[?2026$p",
            "\x1b[>q\x1b[16t",
            "\x1bP+q524742\x1b\\",
            "\x1bPtmux;\x1b\x1b]9;done\x07\x1b\\",
            "\x1b]11;?\x1b\\",
        ] {
            assert_eq!(g(ok, Origin::Own), (ok.to_string(), 0), "{ok:?}");
        }
        // a control byte inside an own payload is still untrusted content
        assert_eq!(g("\x1b]2;a\u{9b}b\x07", Origin::Own).1, 1);
        assert_eq!(g("\x1bPtmux;\x1b\x1b]2;x\x1b\x1bPq\x1b\x1b\\\x1b\\", Origin::Own).1, 1);
        // frames can't use own-only forms
        assert_eq!(g("\x1b[6n", Origin::Frame).1, 1);
        for query in ["\x1b[>q", "\x1b[16t"] {
            assert_eq!(g(query, Origin::Frame).1, 1);
        }
        for operation in ["\x1b[8;40;100t", "\x1b[2q"] {
            assert_eq!(g(operation, Origin::Own).1, 1);
        }
    }

    #[test]
    fn own_sequences_carry_only_cleaned_content() {
        let s = notification_sequence("mare", "done\x1b]52;c;ZXZpbA==\x07 \u{202E}x", "osc9");
        assert_eq!(s.matches('\x1b').count(), 1, "{s:?}");
        assert!(s.contains("done x"), "{s:?}");
        assert_eq!(base64_encode(b"hi!"), "aGkh");
        assert_eq!(base64_encode(b"h"), "aA==");
        assert_eq!(osc52(&"x".repeat(OSC52_MAX + 1)), mare_shared::ui::Osc52Status::TooLarge);
    }

    #[test]
    fn notifications_ring_the_bell_only_when_asked() {
        let osc = notify_bytes("mare", "done", "osc9", false);
        // one BEL (the OSC terminator), no extra ring; tmux / screen passthrough may wrap it
        assert_eq!(osc, notification_sequence("mare", "done", "osc9").into_bytes());
        assert_eq!(osc.iter().filter(|b| **b == 0x07).count(), 1, "{osc:?}");
        assert_eq!(notify_bytes("mare", "done", "osc777", true).last(), Some(&0x07));
        assert_eq!(notify_bytes("mare", "done", "osc777", false).last(), Some(&b'\\'));
        assert_eq!(notify_bytes("mare", "done", "bel", false), b"\x07");
        // all of them pass the write exit as mare's own output
        for b in [osc, notify_bytes("mare", "done", "bel", false), notify_bytes("m", "d", "osc99", true)] {
            let s = String::from_utf8(b).unwrap();
            assert_eq!(g(&s, Origin::Own), (s.clone(), 0), "{s:?}");
        }
    }

    #[test]
    fn image_sequences_pass_only_when_core_built_them() {
        let kitty = "\x1b_Ga=T,f=100,q=2;AAAA\x1b\\";
        let iterm = "\x1b]1337;File=inline=1:AAAA\x07";
        let sixel = "\x1bP9;1q#0;2;0;0;0#0~$\x1b\\";
        // from model / tool / file text (not registered): dropped, frames and own writes alike
        for seq in [kitty, iterm, sixel] {
            let framed = format!("a{seq}b");
            assert_eq!(g(&framed, Origin::Frame), ("ab".to_string(), 1), "{seq:?}");
            assert_eq!(g(&framed, Origin::Own).0, "ab", "{seq:?}");
        }
        // built by the image primitive: registered, so exactly these bytes pass
        for seq in [kitty, iterm, sixel] {
            allow_image(seq.as_bytes());
            let framed = format!("a{seq}b");
            assert_eq!(g(&framed, Origin::Frame), (framed.clone(), 0), "{seq:?}");
        }
        // a different payload of the same kind still doesn't
        assert_eq!(g("\x1b_Ga=T,f=100,q=2;BBBB\x1b\\", Origin::Frame).1, 1);
    }

    #[test]
    fn screen_passthrough_chunks_at_252_and_guard_validates_reassembled_dcs() {
        let long_body = "x".repeat(300);
        let osc = format!("\x1b]9;{long_body}\x1b\\");
        let wrapped = screen_passthrough(&osc);
        // Inner ST became BEL and every DCS payload chunk is <= 252 bytes.
        let mut chunks = Vec::new();
        for part in wrapped.split("\x1b\\") {
            if part.is_empty() {
                continue;
            }
            let body = part.strip_prefix("\x1bP").expect("screen DCS prefix");
            assert!(body.len() <= SCREEN_DCS_CHUNK, "chunk len {} > 252", body.len());
            assert!(!body.contains("\x1b\\"));
            chunks.push(body);
        }
        assert!(chunks.len() >= 2);
        assert_eq!(g(&wrapped, Origin::Own), (wrapped.clone(), 0));

        // Splitting an unsafe sequence across two <=252-byte screen DCS chunks is still caught.
        let smuggled = screen_passthrough("\x1b]2;safe\x1bPq\x07");
        assert_eq!(g(&smuggled, Origin::Own).1, 1);
    }

    #[test]
    fn osc52_is_suppressed_while_handed_off() {
        let _g = test_lock();
        capture(true);
        crate::modes::enter();
        crate::modes::leave();
        let _ = capture(true);
        assert_eq!(osc52("secret"), mare_shared::ui::Osc52Status::HandedOff);
        let while_handed_off = capture(true);
        assert!(while_handed_off.is_empty(), "osc52 must not write while handed off: {while_handed_off:?}");
        crate::modes::resume();
        let _ = capture(true);
        assert_eq!(osc52("ok"), mare_shared::ui::Osc52Status::Written);
        let after_resume = String::from_utf8(capture(false)).unwrap();
        assert!(after_resume.contains("\x1b]52;c;"), "osc52 must work after resume: {after_resume:?}");
        crate::modes::leave();
        capture(false);
    }

    #[cfg(unix)]
    #[test]
    fn write_fd_bounded_times_out_on_full_blocking_pipe_and_restores_flags() {
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (rfd, wfd) = (fds[0], fds[1]);

        // Fill the pipe buffer completely using temporary O_NONBLOCK, then restore blocking mode.
        let orig_flags = unsafe { libc::fcntl(wfd, libc::F_GETFL) };
        assert!(orig_flags >= 0 && (orig_flags & libc::O_NONBLOCK) == 0);
        assert_eq!(unsafe { libc::fcntl(wfd, libc::F_SETFL, orig_flags | libc::O_NONBLOCK) }, 0);
        let chunk = [b'x'; 4096];
        let mut filled = 0usize;
        loop {
            let n = unsafe { libc::write(wfd, chunk.as_ptr().cast(), chunk.len()) };
            if n > 0 {
                filled += n as usize;
            } else {
                assert_eq!(std::io::Error::last_os_error().kind(), std::io::ErrorKind::WouldBlock);
                break;
            }
        }
        assert!(filled > 0);
        // Put the write fd back into blocking mode before calling write_fd_bounded.
        assert_eq!(unsafe { libc::fcntl(wfd, libc::F_SETFL, orig_flags) }, 0);

        let started = Instant::now();
        let wrote = write_fd_bounded(wfd, b"\x1b[<1u\x1b[?2004l", Duration::from_millis(40));
        let elapsed = started.elapsed();
        assert!(!wrote, "full blocking pipe must time out without blocking indefinitely");
        assert!(elapsed < Duration::from_millis(500), "timeout bound exceeded: {elapsed:?}");

        let flags_after = unsafe { libc::fcntl(wfd, libc::F_GETFL) };
        assert_eq!(flags_after & libc::O_NONBLOCK, 0, "original blocking fd flags must be restored");

        // Drain the pipe and verify a subsequent bounded write delivers every byte intact.
        let mut drain = vec![0u8; filled];
        let mut drained = 0usize;
        while drained < filled {
            let n = unsafe { libc::read(rfd, drain[drained..].as_mut_ptr().cast(), filled - drained) };
            assert!(n > 0);
            drained += n as usize;
        }
        let payload = b"\x1b[<1u\x1b[?2004l";
        assert!(write_fd_bounded(wfd, payload, Duration::from_millis(100)));
        let mut got = vec![0u8; payload.len()];
        let n = unsafe { libc::read(rfd, got.as_mut_ptr().cast(), got.len()) };
        assert_eq!(n as usize, payload.len());
        assert_eq!(got, payload);

        unsafe {
            libc::close(rfd);
            libc::close(wfd);
        }
    }
}
