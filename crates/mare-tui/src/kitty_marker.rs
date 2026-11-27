//! Ownership markers for the kitty keyboard entry mare pushes (when a crash or a SIGKILL
//! leaves the terminal in kitty mode).
//!
//! The terminal keeps the pushed entries on a stack that outlives the process, so a mare that
//! died after its push leaves one entry behind on that terminal. Popping blindly at startup
//! could remove the *parent's* entry, so mare only pops what it can prove it pushed:
//!
//! - when mare pushes, it writes a marker file `<state>/kitty/<tty>.<pid>` (one per process, so
//!   a mare inside another mare on the same terminal doesn't hide the outer one's marker);
//! - when mare pops (exit, handoff), it removes its own marker;
//! - at startup, markers for the same tty whose process is gone are a push that never got its
//!   pop: pop that many entries (one per dead marker), delete the markers, then push as usual.
//!   Markers of live processes and of other terminals are left alone.
//!
//! A reused tty name could make a stale marker match a fresh terminal; popping an entry that
//! isn't there is a no-op for the terminal, so the worst case is one pop too many on a terminal
//! whose previous owner crashed and whose new tab pushed its own entry before mare started.
//! Nothing here exists on Windows (no tty name, no kitty protocol in the console).

use std::path::{Path, PathBuf};

use crate::modes::{KITTY_POP, KITTY_PUSH};

/// Never pop more than this many stale entries at startup (a runaway directory can't flood the tty).
const MAX_STALE: usize = 8;

fn marker_dir() -> PathBuf {
    mare_shared::paths::state_dir().join("kitty")
}

/// A file-name-safe key for the controlling terminal (`/dev/ttys003` -> `_dev_ttys003`).
#[cfg(any(unix, test))]
fn sanitize(tty: &str) -> String {
    tty.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

#[cfg(all(unix, not(test)))]
fn tty_key() -> Option<String> {
    for fd in [1, 0, 2] {
        // SAFETY: ttyname returns a pointer to a static buffer (or null); copied right away
        let p = unsafe { libc::ttyname(fd) };
        if !p.is_null() {
            let s = unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned();
            return Some(sanitize(&s));
        }
    }
    None
}

#[cfg(any(not(unix), test))]
fn tty_key() -> Option<String> {
    None
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // signal 0 only checks existence; EPERM still means the process exists
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn marker_path(dir: &Path, key: &str, pid: u32) -> PathBuf {
    dir.join(format!("{key}.{pid}"))
}

/// Dead processes' markers for this terminal, removed; the count is how many entries to pop.
fn reclaim_in(dir: &Path, key: &str, own_pid: u32, alive: &dyn Fn(u32) -> bool) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let prefix = format!("{key}.");
    let mut n = 0;
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.strip_prefix(&prefix)).and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if pid == own_pid || alive(pid) {
            continue;
        }
        // count only what we actually removed: a racing second mare must not double-pop
        if std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    n.min(MAX_STALE)
}

/// The bytes for pushing our entry: a pop per stale marker first, then the push; our own
/// marker is written before the push goes out.
fn push_bytes_in(dir: &Path, key: &str, own_pid: u32, alive: &dyn Fn(u32) -> bool) -> Vec<u8> {
    let stale = reclaim_in(dir, key, own_pid, alive);
    let _ = mare_shared::private::create_dir_all(dir);
    let _ = mare_shared::private::open(
        std::fs::OpenOptions::new().create(true).write(true).truncate(true),
        &marker_path(dir, key, own_pid),
    );
    let mut b = KITTY_POP.repeat(stale);
    b.extend_from_slice(KITTY_PUSH);
    b
}

/// Our entry was popped: the marker goes away.
fn popped_in(dir: &Path, key: &str, own_pid: u32) {
    let _ = std::fs::remove_file(marker_path(dir, key, own_pid));
}

/// Bytes to write when mare pushes its kitty entry (stale pops, then the push); records ownership.
pub fn push_bytes() -> Vec<u8> {
    match tty_key() {
        Some(key) => push_bytes_in(&marker_dir(), &key, std::process::id(), &pid_alive),
        None => KITTY_PUSH.to_vec(),
    }
}

/// Call after mare's own pop was written.
pub fn popped() {
    if let Some(key) = tty_key() {
        popped_in(&marker_dir(), &key, std::process::id());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mare-kitty-marker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    const OWN: u32 = 1000;

    #[cfg(unix)]
    #[test]
    fn kitty_marker_creates_private_directory_and_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("kitty");
        push_bytes_in(&dir, "dev_ttys003", OWN, &|_| false);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
            std::fs::metadata(marker_path(&dir, "dev_ttys003", OWN)).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_dead_process_marker_is_popped_once_before_our_push() {
        let d = tmp("dead");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(marker_path(&d, "dev_ttys003", 4242), b"").unwrap();
        let b = push_bytes_in(&d, "dev_ttys003", OWN, &|_| false);
        assert_eq!(b, b"\x1b[<1u\x1b[>5u", "one stale pop, then our push");
        assert!(!marker_path(&d, "dev_ttys003", 4242).exists(), "stale marker consumed");
        assert!(marker_path(&d, "dev_ttys003", OWN).exists(), "our own push is recorded");
        // the next start on this terminal (our process crashed again, not popped) finds exactly ours
        let b = push_bytes_in(&d, "dev_ttys003", OWN + 1, &|p| p != OWN);
        assert_eq!(b, b"\x1b[<1u\x1b[>5u");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn nothing_is_popped_without_proof_we_pushed() {
        let d = tmp("none");
        // no marker at all: the parent's entry (if any) is not ours to pop
        assert_eq!(push_bytes_in(&d, "dev_ttys003", OWN, &|_| false), b"\x1b[>5u");
        popped_in(&d, "dev_ttys003", OWN);
        // a live mare's marker, another terminal's marker, our own marker: all left alone
        std::fs::write(marker_path(&d, "dev_ttys003", 7), b"").unwrap();
        std::fs::write(marker_path(&d, "dev_ttys009", 8), b"").unwrap();
        std::fs::write(marker_path(&d, "dev_ttys003", OWN), b"").unwrap();
        let alive = |p: u32| p == 7;
        assert_eq!(push_bytes_in(&d, "dev_ttys003", OWN, &alive), b"\x1b[>5u");
        assert!(marker_path(&d, "dev_ttys003", 7).exists() && marker_path(&d, "dev_ttys009", 8).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn one_pop_per_stale_marker_and_a_normal_exit_leaves_none() {
        let d = tmp("many");
        std::fs::create_dir_all(&d).unwrap();
        for pid in [11, 12] {
            std::fs::write(marker_path(&d, "dev_ttys003", pid), b"").unwrap();
        }
        let b = push_bytes_in(&d, "dev_ttys003", OWN, &|_| false);
        assert_eq!(b, b"\x1b[<1u\x1b[<1u\x1b[>5u");
        popped_in(&d, "dev_ttys003", OWN);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0, "a clean pop removes the marker");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tty_names_become_file_names() {
        assert_eq!(sanitize("/dev/ttys003"), "_dev_ttys003");
        assert_eq!(sanitize("/dev/pts/12"), "_dev_pts_12");
    }
}
