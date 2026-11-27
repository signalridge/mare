//! `mare.clipboard.copy(text) -> {ok, method, error?, verified}`: one clipboard
//! back end for `/copy`, overlays and selection copy (Apache-2.0); the order:
//!
//! 1. **SSH session** (`SSH_TTY` / `SSH_CONNECTION`): the native clipboard belongs to the remote
//!    machine, so use tmux's clipboard (`tmux load-buffer -w -`) inside tmux, otherwise OSC 52.
//! 2. **Local session**: the native clipboard (`arboard`) first; on WSL fall back to PowerShell's
//!    `Set-Clipboard`; finally the terminal path (tmux, then OSC 52).
//!
//! Every copy detects afresh (a clipboard tool installed after startup is found). On Linux the
//! `arboard` handle is kept until mare exits (X11 / some Wayland compositors serve the contents
//! from the writing process). OSC 52 goes through the TUI's single write
//! exit (`UiShared::osc52`) or, without a TUI, straight to `/dev/tty` — never stdout,
//! which carries headless output. An OSC 52 copy can't be confirmed: `verified = false`.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mare_shared::ui::UiShared;
use mlua::{Lua, Table};

/// Maximum raw bytes put into an OSC 52 sequence.
pub const OSC52_MAX_RAW_BYTES: usize = 100_000;

/// Maximum time a clipboard helper subprocess (`powershell.exe`, `tmux`) may run before being
/// killed so copy can fall back to OSC 52 instead of hanging forever.
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(3);

/// Which path put the text on a clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Native,
    Tmux,
    Osc52,
    Wsl,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Tmux => "tmux",
            Self::Osc52 => "osc52",
            Self::Wsl => "wsl",
        }
    }
}

/// The environment the choice depends on (detected on every copy).
#[derive(Clone, Copy, Debug, Default)]
pub struct CopyEnvironment {
    pub ssh: bool,
    pub wsl: bool,
    pub tmux: bool,
}

impl CopyEnvironment {
    pub fn detect() -> Self {
        let var = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        Self { ssh: var("SSH_TTY") || var("SSH_CONNECTION"), wsl: is_wsl(), tmux: var("TMUX") || var("TMUX_PANE") }
    }
}

/// The copy back ends, injectable so the choice is tested without touching a real clipboard.
pub struct Backends<'a> {
    pub native: &'a dyn Fn(&str) -> Result<(), String>,
    pub wsl: &'a dyn Fn(&str) -> Result<(), String>,
    pub tmux: &'a dyn Fn(&str) -> Result<(), String>,
    pub osc52: &'a dyn Fn(&str) -> Result<(), String>,
}

/// Pick and run the clipboard copy path for `env`; the error lists every attempt.
pub fn copy_with(text: &str, env: CopyEnvironment, b: &Backends) -> Result<Method, String> {
    if env.ssh {
        // the native clipboard would write to the remote machine
        return terminal_copy(text, env.tmux, b).map_err(|e| {
            if env.tmux {
                format!("terminal clipboard copy failed over SSH: {e}")
            } else {
                format!("OSC 52 clipboard copy failed over SSH: {e}")
            }
        });
    }
    let native_err = match (b.native)(text) {
        Ok(()) => return Ok(Method::Native),
        Err(e) => e,
    };
    if env.wsl {
        let wsl_err = match (b.wsl)(text) {
            Ok(()) => return Ok(Method::Wsl),
            Err(e) => e,
        };
        return terminal_copy(text, env.tmux, b).map_err(|e| {
            let term = if env.tmux { "terminal" } else { "OSC 52" };
            format!("native clipboard: {native_err}; WSL fallback: {wsl_err}; {term} fallback: {e}")
        });
    }
    terminal_copy(text, env.tmux, b).map_err(|e| {
        let term = if env.tmux { "terminal" } else { "OSC 52" };
        format!("native clipboard: {native_err}; {term} fallback: {e}")
    })
}

/// Through the terminal, preferring tmux's own clipboard (no DCS passthrough needed).
fn terminal_copy(text: &str, tmux: bool, b: &Backends) -> Result<Method, String> {
    if tmux {
        match (b.tmux)(text) {
            Ok(()) => return Ok(Method::Tmux),
            Err(tmux_err) => {
                return (b.osc52)(text)
                    .map(|()| Method::Osc52)
                    .map_err(|e| format!("tmux clipboard: {tmux_err}; OSC 52 fallback: {e}"));
            }
        }
    }
    (b.osc52)(text).map(|()| Method::Osc52)
}

#[cfg(target_os = "linux")]
fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|r| r.to_ascii_lowercase().contains("microsoft"))
}

#[cfg(not(target_os = "linux"))]
fn is_wsl() -> bool {
    false
}

// ---- back ends ----------------------------------------------------------------------------------

/// Linux: the handle that serves the clipboard contents, kept until mare exits.
#[cfg(target_os = "linux")]
static LEASE: std::sync::Mutex<Option<arboard::Clipboard>> = std::sync::Mutex::new(None);

/// Read text when present; an incompatible or empty clipboard is not an error.
pub fn read_text() -> anyhow::Result<Option<String>> {
    let mut clipboard = arboard::Clipboard::new()?;
    match clipboard.get_text() {
        Ok(text) => Ok(Some(text)),
        Err(arboard::Error::ContentNotAvailable) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub fn native_copy(text: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let _quiet = SuppressStderr::new();
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
    cb.set_text(text).map_err(|e| format!("failed to set clipboard text: {e}"))?;
    #[cfg(target_os = "linux")]
    {
        *LEASE.lock().unwrap_or_else(|p| p.into_inner()) = Some(cb);
    }
    Ok(())
}

fn kill_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: `child.id()` is the process group leader created by `process_group(0)`.
        let pid = child.id() as libc::pid_t;
        if pid > 0 {
            unsafe {
                let _ = libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Wait for `child` up to `timeout`, draining pipes on helper threads so a full pipe cannot
/// deadlock and killing + reaping the process (and its Unix process group) on timeout.
fn wait_child_with_timeout(
    mut child: std::process::Child,
    cmd: &str,
    stdin_bytes: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let stdin_handle = if let Some(bytes) = stdin_bytes {
        let Some(mut stdin) = child.stdin.take() else {
            kill_child(&mut child);
            return Err(format!("failed to open {cmd} stdin"));
        };
        Some(std::thread::spawn(move || stdin.write_all(&bytes)))
    } else {
        None
    };
    let stdout_handle = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            buf
        })
    });
    let stderr_handle = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf);
            buf
        })
    });

    const CHILD_POLL_INITIAL: Duration = Duration::from_millis(2);
    const CHILD_POLL_MAX: Duration = Duration::from_millis(20);
    let deadline = Instant::now() + timeout;
    let mut poll = CHILD_POLL_INITIAL;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let now = Instant::now();
                if now >= deadline {
                    kill_child(&mut child);
                    // Do not block joining pipe threads if a surviving descendant still holds FDs.
                    drop((stdin_handle, stdout_handle, stderr_handle));
                    return Err(format!("{cmd} timed out after {}ms", timeout.as_millis()));
                }
                std::thread::sleep(poll.min(deadline.saturating_duration_since(now)));
                poll = (poll * 2).min(CHILD_POLL_MAX);
            }
            Err(e) => {
                kill_child(&mut child);
                drop((stdin_handle, stdout_handle, stderr_handle));
                return Err(format!("failed to wait for {cmd}: {e}"));
            }
        }
    };

    let write_err = stdin_handle.and_then(|h| h.join().ok()).and_then(Result::err);
    let stdout = stdout_handle.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = stderr_handle.and_then(|h| h.join().ok()).unwrap_or_default();
    if let Some(e) = write_err
        && !status.success()
    {
        return Err(format!("failed to write to {cmd}: {e}"));
    }
    Ok(std::process::Output { status, stdout, stderr })
}

fn spawn_helper(cmd: &str, args: &[&str], pipe_stdin: bool, pipe_stdout: bool) -> Result<std::process::Child, String> {
    let mut command = std::process::Command::new(cmd);
    command
        .args(args)
        .stdin(if pipe_stdin { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stdout(if pipe_stdout { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().map_err(|e| format!("failed to spawn {cmd}: {e}"))
}

fn pipe_to_with_timeout(cmd: &str, args: &[&str], text: &str, timeout: Duration) -> Result<(), String> {
    let child = spawn_helper(cmd, args, true, false)?;
    let out = wait_child_with_timeout(child, cmd, Some(text.as_bytes().to_vec()), timeout)?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() { format!("{cmd} exited with {}", out.status) } else { format!("{cmd} failed: {err}") })
}

/// Run `cmd args` with `text` on stdin; the error carries its stderr.
fn pipe_to(cmd: &str, args: &[&str], text: &str) -> Result<(), String> {
    pipe_to_with_timeout(cmd, args, text, SUBPROCESS_TIMEOUT)
}

/// The Windows clipboard from a WSL process.
pub fn wsl_copy(text: &str) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("WSL clipboard fallback unavailable on this platform".into());
    }
    pipe_to(
        "powershell.exe",
        &[
            "-NoProfile",
            "-Command",
            "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; $ErrorActionPreference = 'Stop'; \
             $text = [Console]::In.ReadToEnd(); Set-Clipboard -Value $text",
        ],
        text,
    )
}

fn command_output_with_timeout(cmd: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let child = spawn_helper(cmd, args, false, true)?;
    let out = wait_child_with_timeout(child, cmd, None, timeout)?;
    if out.status.success() {
        return String::from_utf8(out.stdout).map_err(|e| format!("{cmd} output was not UTF-8: {e}"));
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() { format!("{cmd} exited with {}", out.status) } else { format!("{cmd} failed: {err}") })
}

fn tmux_output(args: &[&str]) -> Result<String, String> {
    command_output_with_timeout("tmux", args, SUBPROCESS_TIMEOUT)
}

/// tmux forwards clipboard writes to the outer terminal only with `set-clipboard` on and the `Ms`
/// capability.
pub fn tmux_ready(
    set_clipboard: Result<String, String>,
    info: impl FnOnce() -> Result<String, String>,
) -> Result<(), String> {
    if set_clipboard?.trim() == "off" {
        return Err("tmux clipboard forwarding is disabled (set -g set-clipboard on)".into());
    }
    if info()?.lines().any(|l| l.contains("Ms: [missing]")) {
        return Err("tmux clipboard forwarding is unavailable: missing Ms capability".into());
    }
    Ok(())
}

/// `tmux load-buffer -w -`: a tmux paste buffer plus the outer terminal's clipboard.
pub fn tmux_copy(text: &str) -> Result<(), String> {
    tmux_ready(tmux_output(&["show-options", "-gv", "set-clipboard"]), || tmux_output(&["info"]))?;
    pipe_to("tmux", &["load-buffer", "-w", "-"], text)
}

/// OSC 52 sequence, tmux-wrapped when asked; over the cap → error.
pub fn osc52_sequence(text: &str, tmux: bool) -> Result<String, String> {
    use base64::Engine;
    if text.len() > OSC52_MAX_RAW_BYTES {
        return Err(format!(
            "OSC 52 payload too large ({} bytes; max {OSC52_MAX_RAW_BYTES}) — write it to a file instead",
            text.len()
        ));
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    Ok(if tmux { format!("\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\") } else { format!("\x1b]52;c;{b64}\x07") })
}

/// OSC 52 without a TUI: to the controlling terminal only (never stdout).
fn osc52_tty(text: &str) -> Result<(), String> {
    let seq = osc52_sequence(text, std::env::var_os("TMUX").is_some())?;
    #[cfg(unix)]
    {
        let mut tty = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/tty")
            .map_err(|e| format!("no terminal for OSC 52: {e}"))?;
        tty.write_all(seq.as_bytes()).and_then(|()| tty.flush()).map_err(|e| format!("failed to write OSC 52: {e}"))
    }
    #[cfg(not(unix))]
    {
        let _ = seq;
        Err("OSC 52 needs the TUI on this platform".into())
    }
}

/// macOS: `NSPasteboard` logs to stderr on first use, which would corrupt the screen.
#[cfg(target_os = "macos")]
struct SuppressStderr(Option<libc::c_int>);

#[cfg(target_os = "macos")]
impl SuppressStderr {
    fn new() -> Self {
        // SAFETY: plain fd juggling on fd 2, restored in Drop
        unsafe {
            let saved = libc::dup(2);
            if saved < 0 {
                return Self(None);
            }
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
            if null < 0 || libc::dup2(null, 2) < 0 {
                if null >= 0 {
                    libc::close(null);
                }
                libc::close(saved);
                return Self(None);
            }
            libc::close(null);
            Self(Some(saved))
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for SuppressStderr {
    fn drop(&mut self) {
        if let Some(saved) = self.0 {
            // SAFETY: restore the fd saved in `new`
            unsafe {
                libc::dup2(saved, 2);
                libc::close(saved);
            }
        }
    }
}

// ---- Lua ----------------------------------------------------------------------------------------

/// Test seam (debug builds only): `MARE_TEST_CLIPBOARD=osc52` skips the native, WSL and tmux paths
/// so the terminal path can be exercised on any machine; `slow:<ms>` sleeps for `<ms>` before
/// falling back to OSC 52 so tests can verify the UI loop stays responsive while a slow backend runs.
fn test_clipboard_mode() -> Option<String> {
    if cfg!(debug_assertions) { std::env::var("MARE_TEST_CLIPBOARD").ok() } else { None }
}

/// Copy with the real back ends; OSC 52 through `ui`'s write exit while the TUI is attached.
pub fn copy(text: &str, ui: &UiShared) -> Result<Method, String> {
    let ui_osc = |t: &str| -> Result<(), String> {
        if ui.connected()
            && let Some(status) = ui.osc52(t)
        {
            return match status {
                mare_shared::ui::Osc52Status::Written => Ok(()),
                mare_shared::ui::Osc52Status::TooLarge => Err(format!(
                    "OSC 52 payload too large ({} bytes; max {OSC52_MAX_RAW_BYTES}) — write it to a file instead",
                    t.len()
                )),
                mare_shared::ui::Osc52Status::HandedOff => {
                    Err("terminal is handed off to an external program (OSC 52 skipped)".into())
                }
            };
        }
        osc52_tty(t)
    };
    // the write exit's own cap is 100 KiB's 100 000 bytes applies first
    let osc = |t: &str| {
        if t.len() > OSC52_MAX_RAW_BYTES {
            return osc52_sequence(t, false).map(|_| ());
        }
        ui_osc(t)
    };
    let off = |_: &str| -> Result<(), String> { Err("skipped (MARE_TEST_CLIPBOARD)".into()) };
    if let Some(mode) = test_clipboard_mode() {
        if mode == "osc52" {
            let env = CopyEnvironment { ssh: true, wsl: false, tmux: false };
            return copy_with(text, env, &Backends { native: &off, wsl: &off, tmux: &off, osc52: &osc });
        }
        if let Some(ms) = mode.strip_prefix("slow:").and_then(|v| v.parse::<u64>().ok()) {
            const TEST_SLOW_UNIT: Duration = Duration::from_millis(1);
            let slow_wsl = |_: &str| -> Result<(), String> {
                std::thread::sleep(TEST_SLOW_UNIT.saturating_mul(ms.min(u32::MAX as u64) as u32));
                Err(format!("slow clipboard backend timed out after {ms}ms"))
            };
            let env = CopyEnvironment { ssh: false, wsl: true, tmux: false };
            return copy_with(text, env, &Backends { native: &off, wsl: &slow_wsl, tmux: &off, osc52: &osc });
        }
    }
    copy_with(
        text,
        CopyEnvironment::detect(),
        &Backends { native: &native_copy, wsl: &wsl_copy, tmux: &tmux_copy, osc52: &osc },
    )
}

pub fn install(lua: &Lua, mare: &Table, ui: Arc<UiShared>) -> mlua::Result<()> {
    let t = lua.create_table()?;
    // copy(text) -> { ok, method = "native"|"tmux"|"osc52"|"wsl", verified, error? } (async:
    // clipboard tools are separate processes). `verified = false` for OSC 52: the terminal may
    // ignore it, so callers never claim "copied" for that path.
    t.set(
        "copy",
        lua.create_async_function(move |lua, text: mlua::LuaString| {
            let ui = ui.clone();
            async move {
                let text = text.to_string_lossy();
                let bytes = text.len();
                // clipboard tools are separate processes: off the Lua thread; OSC 52 still goes
                // through the write exit (a channel send) while the TUI is attached
                let r = tokio::task::spawn_blocking(move || copy(&text, &ui))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r);
                let out = lua.create_table()?;
                out.set("bytes", bytes)?;
                match r {
                    Ok(m) => {
                        out.set("ok", true)?;
                        out.set("method", m.name())?;
                        out.set("verified", m != Method::Osc52)?;
                    }
                    Err(e) => {
                        out.set("ok", false)?;
                        out.set("error", e)?;
                    }
                }
                Ok(out)
            }
        })?,
    )?;
    t.set("max_osc52_bytes", OSC52_MAX_RAW_BYTES)?;
    mare.set("clipboard", t)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        calls: RefCell<Vec<&'static str>>,
    }

    fn run(env: CopyEnvironment, ok: &[&'static str]) -> (Result<Method, String>, Vec<&'static str>) {
        let f = Fake { calls: RefCell::new(vec![]) };
        let mk = |name: &'static str| {
            let f = &f;
            let ok = ok.contains(&name);
            move |_: &str| -> Result<(), String> {
                f.calls.borrow_mut().push(name);
                if ok { Ok(()) } else { Err(format!("{name} failed")) }
            }
        };
        let (n, w, t, o) = (mk("native"), mk("wsl"), mk("tmux"), mk("osc52"));
        let r = copy_with("x", env, &Backends { native: &n, wsl: &w, tmux: &t, osc52: &o });
        let calls = f.calls.borrow().clone();
        (r, calls)
    }

    #[test]
    fn ssh_never_touches_the_native_clipboard() {
        let ssh = CopyEnvironment { ssh: true, wsl: false, tmux: false };
        assert_eq!(run(ssh, &["native", "osc52"]), (Ok(Method::Osc52), vec!["osc52"]));
        let ssh_tmux = CopyEnvironment { tmux: true, ..ssh };
        assert_eq!(run(ssh_tmux, &["tmux", "osc52"]), (Ok(Method::Tmux), vec!["tmux"]));
        // tmux refused (set-clipboard off) → OSC 52
        assert_eq!(run(ssh_tmux, &["osc52"]), (Ok(Method::Osc52), vec!["tmux", "osc52"]));
    }

    #[test]
    fn local_prefers_native_then_wsl_then_the_terminal() {
        let local = CopyEnvironment::default();
        assert_eq!(run(local, &["native", "osc52"]), (Ok(Method::Native), vec!["native"]));
        assert_eq!(run(local, &["osc52"]), (Ok(Method::Osc52), vec!["native", "osc52"]));
        let wsl = CopyEnvironment { wsl: true, ..local };
        assert_eq!(run(wsl, &["wsl", "osc52"]), (Ok(Method::Wsl), vec!["native", "wsl"]));
        let (r, calls) = run(CopyEnvironment { tmux: true, ..wsl }, &[]);
        assert_eq!(calls, vec!["native", "wsl", "tmux", "osc52"]);
        let e = r.unwrap_err();
        assert!(e.contains("native clipboard") && e.contains("WSL fallback") && e.contains("terminal fallback"), "{e}");
    }

    #[test]
    fn osc52_sequence_is_capped_and_tmux_wrapped() {
        assert_eq!(osc52_sequence("hi", false).unwrap(), "\x1b]52;c;aGk=\x07");
        assert_eq!(osc52_sequence("hi", true).unwrap(), "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\");
        let e = osc52_sequence(&"x".repeat(OSC52_MAX_RAW_BYTES + 1), false).unwrap_err();
        assert!(e.contains("too large") && e.contains("file"), "{e}");
    }

    #[test]
    fn tmux_readiness_needs_set_clipboard_and_ms() {
        assert!(tmux_ready(Ok("on\n".into()), || Ok("Ms: [present]".into())).is_ok());
        assert!(tmux_ready(Ok("off\n".into()), || Ok(String::new())).unwrap_err().contains("set-clipboard"));
        assert!(tmux_ready(Ok("external".into()), || Ok("  Ms: [missing]".into())).unwrap_err().contains("Ms"));
    }

    #[cfg(unix)]
    #[test]
    fn hung_subprocess_times_out_and_falls_back_to_osc52() {
        let started = Instant::now();
        let wsl_err = RefCell::new(None);
        let native = |_: &str| -> Result<(), String> { Err("no native".into()) };
        let wsl = |text: &str| -> Result<(), String> {
            let res = pipe_to_with_timeout("sh", &["-c", "sleep 30"], text, Duration::from_millis(80));
            if let Err(ref e) = res {
                *wsl_err.borrow_mut() = Some(e.clone());
            }
            res
        };
        let tmux = |_: &str| -> Result<(), String> { Err("no tmux".into()) };
        let osc52 = |_: &str| -> Result<(), String> { Ok(()) };
        let env = CopyEnvironment { ssh: false, wsl: true, tmux: false };
        let method = copy_with("hello", env, &Backends { native: &native, wsl: &wsl, tmux: &tmux, osc52: &osc52 });
        assert_eq!(method, Ok(Method::Osc52));
        assert!(started.elapsed() < Duration::from_secs(2), "timeout should kill hung helper promptly");
        assert!(wsl_err.borrow().as_deref().unwrap_or("").contains("timed out"), "{:?}", wsl_err.borrow());

        let out_err = command_output_with_timeout("sh", &["-c", "sleep 30"], Duration::from_millis(80)).unwrap_err();
        assert!(out_err.contains("timed out"), "{out_err}");
    }
}
