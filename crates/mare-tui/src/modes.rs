//! Terminal modes the TUI turns on, and handing the terminal back.
//!
//! - **Kitty keyboard protocol** is pushed only after the startup probe saw the terminal answer
//!   `CSI ? u`, and an ownership flag makes sure mare pops exactly the one entry it pushed,
//!   once: an editor or a parent program may have
//!   pushed its own.
//! - **Handoff** (Ctrl+G external editor, Ctrl+Z): every mode mare set is turned off and our
//!   kitty entry popped before the other program runs; while it runs, an exit signal writes no
//!   reset sequences (they would pop the editor's own entry); coming back re-enters every mode
//!   .
//! - **Exit** resets what mare may have changed:
//!   SGR, cursor visibility, alternate screen (only if mare is in it), mouse and paste modes,
//!   focus reporting, our kitty entry. Startup never pops blindly.
//! - **Crash leftovers**: a marker file per push (`kitty_marker`) lets the next start pop the
//!   entry a dead mare left on this terminal, and only that.
//! - **Hangup**: a closed terminal that sent no SIGHUP shows up as EOF / EIO on stdin, which ends
//!   the TUI like SIGHUP (no polling).
//!
//! Byte builders are pure (tests read them); the bytes go through the single write exit.

use std::sync::atomic::{AtomicBool, Ordering};

static KITTY_SUPPORTED: AtomicBool = AtomicBool::new(false);
static KITTY_PUSHED: AtomicBool = AtomicBool::new(false);
static ALT_KITTY_PUSHED: AtomicBool = AtomicBool::new(false);
static HANDED_OFF: AtomicBool = AtomicBool::new(false);
static IN_ALT: AtomicBool = AtomicBool::new(false);
static WAS_IN_ALT: AtomicBool = AtomicBool::new(false);
static MOUSE: AtomicBool = AtomicBool::new(false);

/// Mouse reporting for the live-area selection: button presses, drags, releases
/// (1002) with SGR coordinates (1006). Off by default — mare doesn't take the mouse from the
/// terminal's own selection unless asked.
pub const MOUSE_ON: &[u8] = b"\x1b[?1002h\x1b[?1006h";
pub const MOUSE_OFF: &[u8] = b"\x1b[?1006l\x1b[?1002l";

/// DISAMBIGUATE_ESCAPE_CODES (1) | REPORT_ALTERNATE_KEYS (4)
pub const KITTY_PUSH: &[u8] = b"\x1b[>5u";
pub const KITTY_POP: &[u8] = b"\x1b[<1u";
const MODES_ON: &[u8] = b"\x1b[?2004h\x1b[?1004h"; // bracketed paste, focus reporting
const LEAVE_IDLE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Bytes that turn mare's modes on (`kitty`: also push our keyboard flags).
pub fn enter_bytes(kitty: bool) -> Vec<u8> {
    let mut b = MODES_ON.to_vec();
    if kitty {
        b.extend_from_slice(KITTY_PUSH);
    }
    b
}

/// Bytes that hand the terminal back: pop alternate Kitty before leaving alt screen (if in it),
/// reset SGR and cursor, leave alt screen, pop main Kitty (only if we pushed it), mouse, paste,
/// focus.
pub fn leave_bytes_full(main_kitty: bool, alt_kitty: bool, in_alt: bool) -> Vec<u8> {
    let mut b = Vec::new();
    if in_alt {
        if alt_kitty {
            b.extend_from_slice(KITTY_POP);
        }
        b.extend_from_slice(b"\x1b[0m\x1b[?25h\x1b[?1049l");
    } else {
        b.extend_from_slice(b"\x1b[0m\x1b[?25h");
    }
    if main_kitty {
        b.extend_from_slice(KITTY_POP);
    }
    b.extend_from_slice(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l");
    b
}

/// Bytes that hand the terminal back (`kitty_pushed` applies to the active screen stacks).
pub fn leave_bytes(kitty_pushed: bool, in_alt: bool) -> Vec<u8> {
    leave_bytes_full(kitty_pushed, kitty_pushed && in_alt, in_alt)
}

/// Startup: raw mode is the caller's; turn on paste + focus reporting.
pub fn enter() {
    HANDED_OFF.store(false, Ordering::SeqCst);
    IN_ALT.store(false, Ordering::SeqCst);
    WAS_IN_ALT.store(false, Ordering::SeqCst);
    ALT_KITTY_PUSHED.store(false, Ordering::SeqCst);
    crate::out::write_own(enter_bytes(false));
}

/// The probe found the kitty keyboard protocol: push our flags (once).
pub fn enable_kitty() {
    KITTY_SUPPORTED.store(true, Ordering::SeqCst);
    if !HANDED_OFF.load(Ordering::SeqCst) {
        if !KITTY_PUSHED.swap(true, Ordering::SeqCst) {
            crate::out::write_own(crate::kitty_marker::push_bytes());
        }
        if IN_ALT.load(Ordering::SeqCst) && !ALT_KITTY_PUSHED.swap(true, Ordering::SeqCst) {
            crate::out::write_own(KITTY_PUSH.to_vec());
        }
    }
}

pub fn kitty_active() -> bool {
    KITTY_PUSHED.load(Ordering::SeqCst)
}

/// The alternate screen was entered / left (Ctrl+T transcript), so exit knows whether to leave it.
/// Manages the alternate screen's own Kitty keyboard stack independently from the main screen.
pub fn set_alt_screen(on: bool) {
    if HANDED_OFF.load(Ordering::SeqCst) {
        WAS_IN_ALT.store(on, Ordering::SeqCst);
        return;
    }
    if IN_ALT.swap(on, Ordering::SeqCst) == on {
        return;
    }
    let mut b = Vec::new();
    if on {
        b.extend_from_slice(b"\x1b[?1049h");
        if KITTY_SUPPORTED.load(Ordering::SeqCst) && !ALT_KITTY_PUSHED.swap(true, Ordering::SeqCst) {
            b.extend_from_slice(KITTY_PUSH);
        }
    } else {
        if ALT_KITTY_PUSHED.swap(false, Ordering::SeqCst) {
            b.extend_from_slice(KITTY_POP);
        }
        b.extend_from_slice(b"\x1b[?1049l");
    }
    crate::out::write_own(b);
}

/// Hand the terminal to another program (editor, the shell on Ctrl+Z) or to the parent on exit.
/// While handed off, calling it again writes nothing.
pub fn leave() {
    if HANDED_OFF.swap(true, Ordering::SeqCst) {
        return;
    }
    let main_kitty = KITTY_PUSHED.swap(false, Ordering::SeqCst);
    let alt_kitty = ALT_KITTY_PUSHED.swap(false, Ordering::SeqCst);
    let alt = IN_ALT.swap(false, Ordering::SeqCst);
    WAS_IN_ALT.store(alt, Ordering::SeqCst);
    crate::out::write_own(leave_bytes_full(main_kitty, alt_kitty, alt));
    if main_kitty {
        crate::kitty_marker::popped();
    }
    crate::out::wait_idle(LEAVE_IDLE_WAIT);
    // the other program (or the parent shell) writes to the terminal again, not into the capture
    crate::capture::pause();
}

/// Back from a handoff: every mode as before (kitty re-pushed if the terminal supports it,
/// alternate screen and its Kitty stack restored if active before handoff).
pub fn resume() {
    HANDED_OFF.store(false, Ordering::SeqCst);
    crate::capture::resume();
    let kitty = KITTY_SUPPORTED.load(Ordering::SeqCst);
    let push_main = kitty && !KITTY_PUSHED.swap(true, Ordering::SeqCst);
    let mut b = enter_bytes(false);
    if push_main {
        b.extend_from_slice(&crate::kitty_marker::push_bytes());
    }
    if WAS_IN_ALT.swap(false, Ordering::SeqCst) {
        IN_ALT.store(true, Ordering::SeqCst);
        b.extend_from_slice(b"\x1b[?1049h");
        if kitty && !ALT_KITTY_PUSHED.swap(true, Ordering::SeqCst) {
            b.extend_from_slice(KITTY_PUSH);
        }
    }
    if MOUSE.load(Ordering::SeqCst) {
        b.extend_from_slice(MOUSE_ON);
    }
    crate::out::write_own(b);
}

/// Turn mouse reporting on or off (select mode); kept across a handoff.
pub fn set_mouse(on: bool) {
    if MOUSE.swap(on, Ordering::SeqCst) != on && !HANDED_OFF.load(Ordering::SeqCst) {
        crate::out::write_own(if on { MOUSE_ON.to_vec() } else { MOUSE_OFF.to_vec() });
    }
}

pub fn mouse_active() -> bool {
    MOUSE.load(Ordering::SeqCst)
}

pub fn handed_off() -> bool {
    HANDED_OFF.load(Ordering::SeqCst)
}

/// Reclaim mode ownership during process shutdown after an external program has already exited.
/// When `leave_alt` is true (e.g. an editor exited abnormally or was force-killed and may have left
/// the alternate screen active), emit safe resets without popping Kitty flags (our own Kitty entry
/// was already popped on `leave()`).
pub fn end_handoff_for_exit(leave_alt: bool) {
    if leave_alt {
        crate::out::write_own(leave_bytes_full(false, false, true));
        crate::out::wait_idle(LEAVE_IDLE_WAIT);
    }
    KITTY_PUSHED.store(false, Ordering::SeqCst);
    ALT_KITTY_PUSHED.store(false, Ordering::SeqCst);
    IN_ALT.store(false, Ordering::SeqCst);
    WAS_IN_ALT.store(false, Ordering::SeqCst);
    HANDED_OFF.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_resets_modes_and_pops_only_our_kitty_entry() {
        let b = String::from_utf8(leave_bytes(true, false)).unwrap();
        assert!(b.starts_with("\x1b[0m\x1b[?25h\x1b[<1u") && b.matches("\x1b[<").count() == 1, "{b:?}");
        assert!(b.contains("\x1b[0m") && b.contains("\x1b[?25h") && b.contains("\x1b[?2004l"), "{b:?}");
        assert!(!b.contains("1049"), "alternate screen left only when we're in it");
        let b = String::from_utf8(leave_bytes(false, true)).unwrap();
        assert!(!b.contains("\x1b[<") && b.contains("\x1b[?1049l"), "never pop an entry we didn't push");
        let b_alt = String::from_utf8(leave_bytes_full(true, true, true)).unwrap();
        let alt_pop = b_alt.find("\x1b[<1u").unwrap();
        let alt_leave = b_alt.find("\x1b[?1049l").unwrap();
        let main_pop = b_alt.rfind("\x1b[<1u").unwrap();
        assert!(alt_pop < alt_leave && alt_leave < main_pop, "{b_alt:?}");
        assert_eq!(enter_bytes(true), b"\x1b[?2004h\x1b[?1004h\x1b[>5u");
    }

    #[test]
    fn mouse_reporting_is_off_until_asked_and_survives_a_handoff() {
        let _g = crate::out::test_lock();
        crate::out::capture(true);
        enter();
        set_mouse(true);
        set_mouse(true); // already on: nothing more
        leave();
        resume();
        set_mouse(false);
        let s = String::from_utf8(crate::out::capture(false)).unwrap();
        assert_eq!(s.matches("\x1b[?1002h").count(), 2, "on, and again after the handoff: {s:?}");
        assert!(s.contains("\x1b[?1002l") && s.ends_with("\x1b[?1006l\x1b[?1002l"), "{s:?}");
        assert!(!s.starts_with("\x1b[?1002h"), "startup doesn't take the mouse: {s:?}");
        leave();
        crate::out::capture(false);
    }

    #[test]
    fn handoff_pops_once_and_writes_nothing_while_handed_off() {
        let _g = crate::out::test_lock();
        crate::out::capture(true);
        enter();
        enable_kitty();
        enable_kitty(); // a second answer doesn't push twice
        leave(); // editor starts
        leave(); // exit signal while the editor runs: nothing more
        resume(); // editor done
        let s = String::from_utf8(crate::out::capture(false)).unwrap();
        assert_eq!(s.matches("\x1b[>5u").count(), 2, "pushed at enable and at resume: {s:?}");
        assert_eq!(s.matches("\x1b[<1u").count(), 1, "popped exactly once: {s:?}");
        leave();
        crate::out::capture(false);
    }

    #[test]
    fn alt_screen_kitty_stack_and_handoff_restore_independently() {
        let _g = crate::out::test_lock();
        crate::out::capture(true);
        enter();
        enable_kitty();
        set_alt_screen(true);
        leave();
        resume();
        set_alt_screen(false);
        let s = String::from_utf8(crate::out::capture(false)).unwrap();
        assert_eq!(s.matches("\x1b[?1049h").count(), 2, "entered + restored on resume: {s:?}");
        assert_eq!(s.matches("\x1b[?1049l").count(), 2, "left on handoff + closed: {s:?}");
        assert_eq!(s.matches("\x1b[>5u").count(), 4, "main + alt + resume main + resume alt: {s:?}");
        assert_eq!(s.matches("\x1b[<1u").count(), 3, "handoff alt + handoff main + close alt: {s:?}");
        leave();
        crate::out::capture(false);
    }

    #[test]
    fn set_alt_screen_while_handed_off_updates_resume_target_without_desync() {
        let _g = crate::out::test_lock();
        crate::out::capture(true);
        enter();
        enable_kitty();
        // Hand off while on main screen, then a background UI command requests alt screen.
        leave();
        let _ = crate::out::capture(true);
        set_alt_screen(true);
        let during_handoff = String::from_utf8(crate::out::capture(true)).unwrap();
        assert!(during_handoff.is_empty(), "must write nothing while handed off: {during_handoff:?}");

        // Resuming must enter alt screen and push alt Kitty; later leaving alt must emit ?1049l.
        resume();
        let on_resume = String::from_utf8(crate::out::capture(true)).unwrap();
        assert!(on_resume.contains("\x1b[?1049h"), "resume must enter deferred alt screen: {on_resume:?}");
        assert_eq!(on_resume.matches("\x1b[>5u").count(), 2, "main + alt kitty pushed on resume: {on_resume:?}");

        // Now hand off from alt screen, close alt screen while handed off, then resume:
        // resume must NOT re-enter alt screen.
        leave();
        let _ = crate::out::capture(true);
        set_alt_screen(false);
        let during_handoff_close = String::from_utf8(crate::out::capture(true)).unwrap();
        assert!(during_handoff_close.is_empty(), "must write nothing while handed off: {during_handoff_close:?}");
        resume();
        let on_resume_main = String::from_utf8(crate::out::capture(false)).unwrap();
        assert!(
            !on_resume_main.contains("\x1b[?1049h"),
            "closed alt screen must not reopen on resume: {on_resume_main:?}"
        );
        assert_eq!(
            on_resume_main.matches("\x1b[>5u").count(),
            1,
            "only main kitty pushed on resume: {on_resume_main:?}"
        );
        leave();
        crate::out::capture(false);
    }
}
