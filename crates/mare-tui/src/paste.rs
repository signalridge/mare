//! Paste handling without (or around) bracketed paste.
//!
//! - [`PasteBurst`]: terminals that don't send bracketed-paste markers deliver a paste as a fast
//!   stream of key events, so an Enter inside it would submit half the paste. Characters are
//!   still inserted as they arrive; when they come faster than a person types, an Enter shortly
//!   after inserts a newline instead of submitting (Copyright 2025 OpenAI, Apache-2.0).
//! - [`PasteGuard`]: Windows Terminal and conhost send one extra Enter right after a bracketed
//!   paste ends; an Enter on its own within 150 ms of a paste is ignored.

use std::time::{Duration, Instant};

/// Minimum characters in a burst before paste detection triggers.
const MIN_CHARS: u16 = 3;
/// Window after a burst during which Enter inserts a newline instead of submitting.
const ENTER_SUPPRESS_WINDOW: Duration = Duration::from_millis(120);
/// Maximum inter-key interval for detecting the start of a paste burst across all platforms
/// (Codex `PASTE_BURST_CHAR_INTERVAL`).
const CHAR_INTERVAL: Duration = Duration::from_millis(8);
/// Maximum idle interval between characters that keeps an already-active paste burst alive
/// (Codex `PASTE_BURST_ACTIVE_IDLE_TIMEOUT`: Windows delivers unbracketed pastes in slower chunks).
#[cfg(not(windows))]
const ACTIVE_IDLE_TIMEOUT: Duration = Duration::from_millis(8);
#[cfg(windows)]
const ACTIVE_IDLE_TIMEOUT: Duration = Duration::from_millis(60);
/// Guard window to ignore a stray Enter right after a bracketed paste.
pub const GUARD_WINDOW: Duration = Duration::from_millis(150);

#[derive(Default, Debug)]
pub struct PasteBurst {
    last_char: Option<Instant>,
    run: u16,
    window_until: Option<Instant>,
}

impl PasteBurst {
    /// A plain character (no Ctrl/Alt) was typed and inserted.
    pub fn on_char(&mut self, now: Instant) {
        self.on_char_with_active_timeout(now, ACTIVE_IDLE_TIMEOUT);
    }

    fn on_char_with_active_timeout(&mut self, now: Instant, active_idle_timeout: Duration) {
        let continuing_active = self.run >= MIN_CHARS
            && self.last_char.is_some_and(|prev| now.saturating_duration_since(prev) <= active_idle_timeout);
        let interval = if continuing_active { active_idle_timeout } else { CHAR_INTERVAL };
        match self.last_char {
            Some(prev) if now.saturating_duration_since(prev) <= interval => self.run = self.run.saturating_add(1),
            _ => self.run = 1,
        }
        self.last_char = Some(now);
        if self.run >= MIN_CHARS {
            self.window_until = Some(now + ENTER_SUPPRESS_WINDOW);
        }
    }

    /// Enter arrived: true = insert a newline (it's part of a paste), false = submit.
    /// A newline keeps the window open for the rest of the paste.
    pub fn enter_is_newline(&mut self, now: Instant) -> bool {
        let in_window = self.window_until.is_some_and(|t| now <= t);
        let right_after_burst =
            self.run >= MIN_CHARS && self.last_char.is_some_and(|t| now.saturating_duration_since(t) <= CHAR_INTERVAL);
        if in_window || right_after_burst {
            self.window_until = Some(now + ENTER_SUPPRESS_WINDOW);
            self.last_char = Some(now);
            true
        } else {
            false
        }
    }

    /// Inside a paste burst right now (single-key shortcuts like `?` must not fire).
    pub fn active(&self, now: Instant) -> bool {
        self.window_until.is_some_and(|t| now <= t)
    }

    /// True once two or more plain characters arrive at paste speed or while a burst window is active.
    pub fn in_burst_sequence(&self, now: Instant) -> bool {
        self.active(now)
            || (self.run >= 2 && self.last_char.is_some_and(|t| now.saturating_duration_since(t) <= CHAR_INTERVAL))
    }

    /// Any other key (arrows, Ctrl/Alt chords, Esc): typing after it starts fresh.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Default, Debug)]
pub struct PasteGuard {
    pasted_at: Option<Instant>,
}

impl PasteGuard {
    /// A bracketed paste ended.
    pub fn pasted(&mut self, now: Instant) {
        self.pasted_at = Some(now);
    }

    /// An Enter on its own: true = it's the terminal's stray Enter, ignore it.
    pub fn swallow_enter(&mut self, now: Instant) -> bool {
        let stray = self.pasted_at.is_some_and(|t| now.saturating_duration_since(t) <= GUARD_WINDOW);
        self.pasted_at = None; // only the first Enter after a paste
        stray
    }

    /// Any other input after the paste: the guard is spent.
    pub fn clear(&mut self) {
        self.pasted_at = None;
    }
}

/// Files dropped on the terminal arrive as a paste of their paths, written the way the terminal
/// quotes them: `'…'` / `"…"`, backslash-escaped (`my\ file.png`), or `file://` URLs
/// (percent-encoded), several separated by spaces or newlines. Returns the paths, or nothing
/// when the text is no such list (a `\` that escapes nothing — a Windows path — stays a `\`).
pub fn dropped_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    let mut chars = text.trim().chars().peekable();
    let finish = |cur: &mut String, out: &mut Vec<String>| {
        let p = std::mem::take(cur);
        out.push(match p.strip_prefix("file://") {
            Some(rest) => percent_decode(rest),
            None => p,
        });
    };
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, '\\') if chars.peek().is_some_and(|n| " '\"()&;$`!*?[]{}#<>|~\\".contains(*n)) => {
                cur.push(chars.next().unwrap_or('\\'));
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    finish(&mut cur, &mut out);
                    started = false;
                }
            }
            (None, c) => {
                cur.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Vec::new();
    }
    if started {
        finish(&mut cur, &mut out);
    }
    out
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_fast_burst_turns_enter_into_a_newline_but_typing_submits() {
        let t0 = Instant::now();
        let mut b = PasteBurst::default();
        for i in 0..5 {
            b.on_char(t0 + MS * i);
        }
        assert!(b.enter_is_newline(t0 + MS * 6), "Enter inside a paste is a newline");
        b.on_char(t0 + MS * 7);
        assert!(b.enter_is_newline(t0 + MS * 8), "the window carries through the whole paste");
        assert!(b.active(t0 + MS * 9));
        // a person typing: slower than the burst interval
        let mut b = PasteBurst::default();
        for i in 0..5u32 {
            b.on_char(t0 + Duration::from_millis(80) * i);
        }
        assert!(!b.enter_is_newline(t0 + Duration::from_millis(400)), "typed text submits");
        // an Enter long after the burst submits too
        let mut b = PasteBurst::default();
        for i in 0..5 {
            b.on_char(t0 + MS * i);
        }
        assert!(!b.enter_is_newline(t0 + Duration::from_millis(500)));
    }

    #[test]
    fn fast_typing_at_30ms_submits_while_windows_active_timeout_only_extends_started_burst() {
        assert_eq!(CHAR_INTERVAL, Duration::from_millis(8));
        let t0 = Instant::now();
        let win_active_timeout = Duration::from_millis(60);

        // Fast human typing (~30 ms/char) + Enter must submit even under Windows active timeout.
        let mut typed = PasteBurst::default();
        for i in 0..4u32 {
            typed.on_char_with_active_timeout(t0 + Duration::from_millis(30) * i, win_active_timeout);
        }
        assert!(!typed.active(t0 + Duration::from_millis(100)));
        assert!(!typed.in_burst_sequence(t0 + Duration::from_millis(100)));
        assert!(!typed.enter_is_newline(t0 + Duration::from_millis(110)), "fast typing + Enter must submit");

        // Once a burst starts at <=8 ms/char, Windows slower chunk spacing (<=60 ms) keeps it active.
        let mut pasted = PasteBurst::default();
        for i in 0..3u32 {
            pasted.on_char_with_active_timeout(t0 + Duration::from_millis(2) * i, win_active_timeout);
        }
        pasted.on_char_with_active_timeout(t0 + Duration::from_millis(44), win_active_timeout);
        pasted.on_char_with_active_timeout(t0 + Duration::from_millis(84), win_active_timeout);
        assert!(pasted.active(t0 + Duration::from_millis(180)));
        assert!(pasted.enter_is_newline(t0 + Duration::from_millis(180)));
    }

    #[test]
    fn dropped_files_paste_as_quoted_escaped_or_url_paths() {
        assert_eq!(dropped_paths("/tmp/a.png"), vec!["/tmp/a.png"]);
        assert_eq!(dropped_paths("'/tmp/my shot.png' "), vec!["/tmp/my shot.png"]);
        assert_eq!(dropped_paths("/tmp/my\\ shot\\(1\\).png /tmp/b.jpg"), vec!["/tmp/my shot(1).png", "/tmp/b.jpg"]);
        assert_eq!(dropped_paths("\"C:\\Users\\me\\a b.png\""), vec!["C:\\Users\\me\\a b.png"]);
        assert_eq!(dropped_paths("C:\\Users\\me\\a.png"), vec!["C:\\Users\\me\\a.png"], "a Windows path keeps `\\`");
        assert_eq!(dropped_paths("file:///tmp/a%20b.png\nfile:///tmp/c.png"), vec!["/tmp/a b.png", "/tmp/c.png"]);
        assert!(dropped_paths("'unclosed").is_empty());
        assert!(dropped_paths("  ").is_empty());
    }

    #[test]
    fn the_stray_enter_right_after_a_bracketed_paste_is_ignored_once() {
        let t0 = Instant::now();
        let mut g = PasteGuard::default();
        g.pasted(t0);
        assert!(g.swallow_enter(t0 + Duration::from_millis(20)));
        assert!(!g.swallow_enter(t0 + Duration::from_millis(30)), "only the first one");
        g.pasted(t0);
        assert!(!g.swallow_enter(t0 + Duration::from_millis(200)), "a deliberate Enter later submits");
    }
}
