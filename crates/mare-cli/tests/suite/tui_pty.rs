//! TUI e2e, floor layer: the real `mare` binary in a pseudo-terminal (mare's own
//! `mare_binding::platform::Terminal`, the one the `pty` primitive runs programs on), after
//! pi `packages/tui/test/exit-restore.test.ts` — only what needs a real process: the terminal is
//! restored on exit, a hangup exits with the session on disk, and Ctrl+C's two levels.
//!
//! Unix: openpty. Windows: a ConPTY (the same `Terminal`, created by mare itself); closing it is
//! the hangup (its console's clients get CTRL_CLOSE_EVENT). ConPTY renders the console's buffer
//! instead of passing mare's bytes through, so byte-level checks differ per platform (marked).

#[cfg(unix)]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::*;

const INPUT_BOX: &str = "╰";

fn start_pty(
    script: Value,
    stty_after: bool,
    setup: &str,
    init_tail: &str,
    extra_env: &[(&str, String)],
) -> PtySession {
    let mut cmd = MareCmd::new().faux(script).setup(setup).init_tail(init_tail).no_term_probe();
    for (k, v) in extra_env {
        cmd = cmd.env(k, v);
    }
    if stty_after {
        let stty = stty_file(cmd.home_path()).to_string_lossy().into_owned();
        let mare = mare_pty_program(&mut Vec::new()).to_string();
        cmd = cmd.shell_wrap(
            "sh",
            vec![
                "-c".to_string(),
                "\"$0\" --set plugins.onboarding.plugins=false; stty -a > \"$1\"".to_string(),
                mare,
                stty,
            ],
        );
    }
    cmd.pty(100, 30)
}

fn stty_file(home: &Path) -> PathBuf {
    home.join("stty-after")
}

#[test]
fn quitting_restores_the_terminal() {
    // Unix: a shell owns the tty and records `stty -a` after mare exits (Windows has no stty: its
    // console mode restore is covered by `mare-tui` unit tests)
    let mut p = start_pty(json!([]), cfg!(unix), "", "", &[]);
    p.wait_text("the input box", |s| s.contains(INPUT_BOX));
    let startup = p.text();
    p.send(b"\x04"); // Ctrl+D on an empty input quits
    p.wait_exit(15);
    // the shell recorded the tty state after mare exited: cooked again
    #[cfg(unix)]
    {
        let end = Instant::now() + Duration::from_secs(WAIT_SECS);
        while !stty_file(p.home_path()).exists() && Instant::now() < end {
            // sleep-ok: polls for the stty file the child writes, until the deadline
            std::thread::sleep(Duration::from_millis(20));
        }
        let stty = std::fs::read_to_string(stty_file(p.home_path())).unwrap_or_default();
        let flags: Vec<&str> = stty.split_whitespace().collect();
        assert!(flags.contains(&"icanon") && flags.contains(&"echo"), "tty left in raw mode: {stty}");
    }
    // the last bytes undo what startup enabled
    let out = p.text();
    let restore = tail(&out, 800);
    for (seq, what) in [("\x1b[?2004l", "bracketed paste off"), ("\x1b[?25h", "cursor shown")] {
        assert!(restore.contains(seq), "missing {what}: {restore:?}");
    }
    // Windows: ConPTY asks its host for focus events itself (the ?1004h at startup is its own)
    // and keeps them; mare's ?1004l stays in the console (its ?2004l right before it gets through)
    if cfg!(unix) && startup.contains("\x1b[?1004h") {
        assert!(restore.contains("\x1b[?1004l"), "focus reporting left on: {restore:?}");
    }
    if startup.contains("\x1b[>") {
        assert!(restore.contains("\x1b[<"), "keyboard enhancement flags not popped: {restore:?}");
    }
}

#[test]
fn a_hangup_exits_with_the_session_on_disk() {
    let mut p = start_pty(json!([{ "text": "remember this answer" }]), false, "", "", &[]);
    p.wait_text("the input box", |s| s.contains(INPUT_BOX));
    p.prompt("keep me", "the answer", |s| s.contains("remember this answer"));
    p.hang_up();
    let _ = p.wait_exit(15);
    let sessions = p.home_path().join("state/sessions");
    let mut found = false;
    for f in walk(&sessions) {
        let body = std::fs::read_to_string(&f).unwrap_or_default();
        if body.contains("keep me") && body.contains("remember this answer") {
            found = true;
        }
    }
    assert!(found, "the session with both messages was persisted under {}", sessions.display());
}

#[test]
fn ctrl_c_interrupts_the_turn_then_quits() {
    let slow = "z".repeat(3000);
    let mut p = start_pty(json!([{ "text": slow, "delay_ms": 20 }]), false, "", "", &[]);
    p.wait_text("the input box", |s| s.contains(INPUT_BOX));
    p.prompt("go slowly", "streaming", |s| s.contains("zzzzzzzz"));
    // first level: interrupts the turn — the stream stops — and mare keeps running
    p.send(b"\x03");
    let zs = |s: &str| s.matches('z').count();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let a = zs(&p.text());
        std::thread::sleep(Duration::from_millis(400));
        if zs(&p.text()) == a {
            break;
        }
        assert!(Instant::now() < end, "the stream kept going after Ctrl+C");
    }
    // the turn ended as interrupted (counting z bytes can't tell: redraws repeat them)
    p.wait_text("the interrupted closing line", |s| s.contains("interrupted"));
    assert!(p.running(), "one Ctrl+C must not quit");
    // second level: on an empty input, Ctrl+C twice quits
    p.send(b"\x03");
    p.send(b"\x03");
    p.wait_exit(15);
}

/// Ctrl+C interrupts a running command — its process group gets the polite
/// stop (SIGTERM, then SIGKILL after the grace period) — and a second Ctrl+C while it is still
/// stopping SIGKILLs the whole group at once. The command (`sh`, which keeps ignored signals
/// ignored in its children, unlike zsh) and its child ignore INT and TERM, so the leader stays
/// and the stop waits out its 3 s grace before SIGKILL — unless forced.
// Unix only: on Windows every stop ends the job at once (`platform::signal_tree`), no grace to force
#[cfg(unix)]
#[test]
fn a_second_ctrl_c_while_stopping_sigkills_the_process_group() {
    let script = json!([
        { "tool_calls": [{ "name": "bash", "arguments": {
            "command": "exec /bin/sh -c 'trap \"\" INT TERM; sleep 60 & echo SLEEPPID=$!; wait'", "description": "stubborn" } }] },
        { "text": "unreachable" }
    ]);
    // a command of its own (a job), not the persistent shell: the job's stop has the 3 s grace.
    // No sandbox, also in this untrusted temp folder: under bwrap (Linux, `--unshare-pid`) `$!`
    // is a pid in the sandbox's namespace, which the probes below can't see from here
    let mut p = start_pty(
        script,
        false,
        "plugins = { tools = { shell_session = \"fresh\" }, permissions = { preset = \"full-access\", untrusted_preset = \"full-access\" } }",
        "",
        &[],
    );
    p.wait_text("the input box", |s| s.contains(INPUT_BOX));
    p.prompt("go", "the command", |s| screen(s).contains("SLEEPPID="));
    let end = Instant::now() + Duration::from_secs(60);
    let pid = loop {
        // the live output under the running command (`echo $!`)
        let sc = screen(&p.text());
        if let Some(pid) = sc
            .split("SLEEPPID=")
            .skip(1)
            .find_map(|r| r.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u32>().ok())
        {
            break pid;
        }
        assert!(Instant::now() < end, "the command never started; screen:\n{}", screen(&p.text()));
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(pid_alive(pid));
    p.send(b"\x03");
    p.wait_text("the stopping hint", |s| s.contains("Ctrl+C again to force"));
    assert!(pid_alive(pid), "INT and TERM are ignored: still running");
    let forced = Instant::now();
    p.send(b"\x03");
    while pid_alive(pid) {
        assert!(
            forced.elapsed() < Duration::from_secs(5),
            "the second Ctrl+C didn't SIGKILL the group; output tail: {:?}",
            tail(&p.text(), 600)
        );
        // sleep-ok: polls the process group until it is gone or the 5 s deadline; no exit event for it
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(p.running(), "forcing the stop doesn't quit");
    // the killed job is reaped asynchronously: until then it still counts as running and Ctrl+D
    // rightly asks "exit and stop it?" — quit once the stopping state is over
    let end = Instant::now() + Duration::from_secs(WAIT_SECS);
    while screen(&p.text()).contains("Ctrl+C again to force") {
        assert!(Instant::now() < end, "the stop never finished; screen:\n{}", screen(&p.text()));
        // sleep-ok: polls the PTY screen until the stop finishes or the deadline
        std::thread::sleep(Duration::from_millis(20));
    }
    p.send(b"\x04");
    p.wait_exit(15);
}

/// The output as the terminal shows it (for failure messages).
fn screen(out: &str) -> String {
    render_screen(out.as_bytes(), 100, 30)
}

/// A startup refusal may already be in the transcript while a retry Enter is still being
/// refused by the host. Observe this config refusal and its restored editor on the live screen.
pub(super) fn config_refusal_restored(screen: &str) -> bool {
    let refused = screen.split("not sent:").skip(1).any(|tail| {
        let paragraph = tail.split("\n\n").next().unwrap_or_default();
        let message = paragraph.split_whitespace().collect::<Vec<_>>().join(" ");
        message.starts_with("init.lua has an error") && message.contains("BROKEN config line")
    });
    refused && screen.lines().any(|line| line.starts_with("│ ❯ hello ") && line.trim_end().ends_with('│'))
}

#[test]
fn conpty_wrapped_box_rows_still_satisfy_config_refusal_restored() {
    let cols = 100usize;
    let pad = |s: &str| {
        let w = s.chars().count();
        format!("{s}{}", " ".repeat(cols - w))
    };
    // Simulate ConPTY writing a space-cleared blank row and consecutive 100-column box rows
    // without `\r\n` between them so vt100 marks each row as auto-wrapped.
    let top = format!("╭{}╮", "─".repeat(cols - 2));
    let mid = format!("│ ❯ hello{}│", " ".repeat(cols - 10));
    let bot = format!("╰{}╯", "─".repeat(cols - 2));
    let status = pad("  PARTIAL · Shift+Tab mode        config error · /reload · Faux Echo");
    let raw = format!(
        "\x1b[20;1H  not sent: init.lua has an error — init.lua:3: BROKEN config line\r\n\
         \x1b[25;1H{blank}{top}{mid}{bot}{status}",
        blank = " ".repeat(cols),
    );
    assert!(config_refusal_restored(&screen(&raw)), "rendered screen:\n{}", screen(&raw));
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
    out
}

/// The terminal disappears without a SIGHUP: stdin is a pty that is *not* mare's controlling
/// terminal (no TIOCSCTTY), so closing the master only shows up as EOF/EIO on reads. mare must
/// quit, not spin. crossterm 0.29 retried that read forever at 100% CPU; termina reports it
/// (`UnexpectedEof`), `mare_tui::tty::Events` hands it to the loop and the app treats it as a hangup.
// Unix only: a tty that isn't the controlling one has no Windows counterpart (closing the
// pseudoconsole is `a_hangup_exits_with_the_session_on_disk`)
#[cfg(unix)]
#[test]
fn a_terminal_gone_without_sighup_still_exits() {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let tmp = home.path().join("tmp");
    std::fs::create_dir(&tmp).unwrap();
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty writes two fds into the out-params; null name/termios/winsize are allowed
    let rc = unsafe {
        libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
    };
    assert_eq!(rc, 0, "openpty");
    // SAFETY: both fds were just created by openpty and are owned here
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // openpty fds are inheritable: mare must not get a copy of the master (it would keep the
    // terminal open), so mark both close-on-exec (stdio is dup2'ed and so not affected)
    for fd in [&master, &slave] {
        // SAFETY: F_SETFD on a valid fd
        unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(fd), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let ws = libc::winsize { ws_row: 30, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCSWINSZ reads one winsize from a valid pointer on a valid fd
    unsafe { libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(&slave), libc::TIOCSWINSZ, &ws) };
    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo"])
        .current_dir(work.path())
        .env("MARE_HOME", home.path())
        .env("TMPDIR", &tmp)
        .env("TMP", &tmp)
        .env("TEMP", &tmp)
        .env("TERM", "xterm-256color")
        .env("MARE_NO_TERM_PROBE", "1")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe; no controlling terminal is acquired afterwards
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    // This raw terminal deliberately bypasses Terminal::spawn to avoid TIOCSCTTY.
    let mut child = ChildGroup(cmd.spawn().unwrap());
    // the Command keeps its Stdio (slave) fds open: drop it with ours, so only mare holds the tty
    drop(cmd);
    drop(slave);
    // read until the input box is drawn, then close the terminal
    let mut file = std::fs::File::from(master);
    let mut seen = Vec::new();
    let end = Instant::now() + Duration::from_secs(60);
    let mut buf = [0u8; 8192];
    let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
    while !String::from_utf8_lossy(&seen).contains(INPUT_BOX) {
        assert!(Instant::now() < end, "no input box: {:?}", tail(&String::from_utf8_lossy(&seen), 400));
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd
        if unsafe { libc::poll(&mut pfd, 1, 50) } > 0 {
            match file.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
    }
    drop(file);
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if child.0.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > end {
            let _ = child.0.kill();
            panic!("mare kept running after its terminal closed without a SIGHUP");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Startup settings error dialog: a failing
/// init.lua asks first — the file and line, the error, three choices. Continue starts the UI with
/// prompts refused; Exit leaves with status 2.
#[test]
fn a_broken_init_lua_asks_before_the_ui() {
    let broken = "\nerror('BROKEN config line')\n";
    let mut p = start_pty(json!([]), false, "", broken, &[]);
    p.wait_text("the dialog", |s| s.contains("Config error") && s.contains("BROKEN config line"));
    let shown = p.text();
    assert!(shown.contains("init.lua has an error") && shown.contains("init.lua:3"), "{}", tail(&shown, 1500));
    assert!(!shown.contains("Ask mare anything"), "the dialog comes before the main UI");
    p.send(b"3");
    p.wait_text("the main UI", |s| s.contains("Ask mare anything"));
    p.prompt("hello", "the config refusal and restored draft", |s| config_refusal_restored(&screen(s)));
    // the refused text is back in the input box: one Ctrl+C clears it, two more quit
    p.send(b"\x03");
    // sleep-ok: gives the TUI the first Ctrl+C as a separate keypress before the next ones
    std::thread::sleep(Duration::from_millis(200));
    p.send(b"\x03\x03");
    p.wait_exit(15);

    let mut p = start_pty(json!([]), false, "", broken, &[]);
    p.wait_text("the dialog", |s| s.contains("Config error"));
    p.send(b"2");
    assert!(!p.wait_exit(15), "Exit leaves with a failure status");
}

/// Open in editor: the editor gets the file (here a script that removes the bad line), the
/// config runs again, and the UI starts with nothing blocked.
#[test]
fn the_config_error_dialog_opens_the_editor_and_rechecks() {
    let dir = tempfile::tempdir().unwrap();
    // the "editor": drops the BROKEN line from the file it is given (its last argument)
    #[cfg(unix)]
    let fixer = {
        let fixer = dir.path().join("fix-config");
        std::fs::write(
            &fixer,
            "#!/bin/sh\nfor f; do :; done\ngrep -v BROKEN \"$f\" > \"$f.new\" && mv \"$f.new\" \"$f\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fixer, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        fixer
    };
    // Windows: a batch file (mare passes just the file: `fix-config` has no known line syntax)
    #[cfg(windows)]
    let fixer = {
        let fixer = dir.path().join("fix-config.cmd");
        std::fs::write(
            &fixer,
            "@echo off\r\nfindstr /v BROKEN \"%~1\" > \"%~1.new\"\r\nmove /y \"%~1.new\" \"%~1\" > nul\r\n",
        )
        .unwrap();
        fixer
    };
    let editor = fixer.to_string_lossy().into_owned();
    let env = [("VISUAL", editor.clone()), ("EDITOR", editor)];
    let mut p = start_pty(json!([{ "text": "fixed and answered" }]), false, "", "error('BROKEN')\n", &env);
    p.wait_text("the dialog", |s| s.contains("Config error"));
    p.send(b"\r");
    p.wait_text("the main UI", |s| s.contains("Ask mare anything"));
    p.prompt("hello", "the answer", |s| s.contains("fixed and answered"));
    // Wait until the turn ends and the idle editor placeholder returns so the two Ctrl+Cs arm and
    // confirm quit rather than interrupting a still-finishing turn.
    p.wait_screen("the turn settled back to idle", |s| {
        s.contains("fixed and answered") && s.contains("Ask mare anything")
    });
    p.send(b"\x03\x03");
    p.wait_exit(15);
}

/// Alt-screen redraw safety: when the `Ctrl+T` transcript overlay is open on the
/// alternate screen, triggering a global `redraw` (`Ctrl+L`) must only repaint the overlay,
/// never emit `CSI 3J` or dump main-screen history onto the alternate screen; closing the
/// overlay leaves the main-screen scrollback intact.
#[cfg(unix)]
#[test]
fn ctrl_l_during_transcript_overlay_redraws_overlay_without_corrupting_main_scrollback() {
    let mut body = String::from("TOP-SCROLLBACK-LINE\n\n");
    for i in 1..=35 {
        use std::fmt::Write as _;
        let _ = write!(body, "history-filler-line-{i}\n\n");
    }
    body.push_str("BOTTOM-ANSWER-LINE");
    let init_tail = "mare.keymap.set('global', '<C-l>', 'redraw')\n";
    let mut p = start_pty(json!([{ "text": body }]), false, "", init_tail, &[]);
    p.wait_text("the input box", |s| s.contains(INPUT_BOX));
    p.prompt("show history", "multi-line answer", |s| screen(s).contains("BOTTOM-ANSWER-LINE"));

    // Open Ctrl+T full-screen transcript overlay (alternate screen ?1049h) and jump to top (`g`).
    p.send(b"\x14");
    p.wait_text("alt-screen entered", |s| s.contains("\x1b[?1049h"));
    p.send(b"g");
    p.wait_text("transcript overlay open at top", |s| screen(s).contains("Transcript (expanded)"));
    let mark = p.text().len();

    // Trigger global redraw via Ctrl+L while on the alternate screen.
    p.send(b"\x0c");
    p.wait_text("overlay repainted after Ctrl+L", |s| {
        let tail = &s[mark.min(s.len())..];
        tail.contains("Transcript (expanded)") || tail.contains("\x1b[3J")
    });
    let after_ctrl_l = p.text();
    let during_alt = &after_ctrl_l[mark.min(after_ctrl_l.len())..];
    assert!(
        !during_alt.contains("\x1b[3J"),
        "redraw during alt-screen overlay must never emit CSI 3J (wipes main scrollback): {:?}",
        tail(during_alt, 600)
    );
    assert!(
        screen(&after_ctrl_l).contains("Transcript (expanded)"),
        "overlay must stay cleanly rendered after Ctrl+L:\n{}",
        screen(&after_ctrl_l)
    );

    // Close the overlay (Esc) and verify the main screen + scrollback are intact.
    p.send(b"\x1b");
    p.wait_text("returned to main screen", |s| {
        let after = &s[mark.min(s.len())..];
        after.contains("\x1b[?1049l") && !screen(s).contains("Transcript (expanded)") && screen(s).contains(INPUT_BOX)
    });
    let full = p.text();
    let mut parser =
        vt100::Parser::new(std::num::NonZeroU16::new(30).unwrap(), std::num::NonZeroU16::new(100).unwrap(), 2000);
    parser.process(full.as_bytes());
    let mut main_history = String::new();
    parser.screen_mut().set_scrollback(usize::MAX);
    let max_sb = parser.screen().scrollback();
    let mut offset = max_sb;
    loop {
        parser.screen_mut().set_scrollback(offset);
        main_history.push_str(&parser.screen().contents());
        main_history.push('\n');
        if offset == 0 {
            break;
        }
        offset = offset.saturating_sub(30);
    }
    assert!(
        main_history.contains("TOP-SCROLLBACK-LINE") && main_history.contains("BOTTOM-ANSWER-LINE"),
        "main screen scrollback must remain intact after alt-screen Ctrl+L:\n{main_history}"
    );
    p.send(b"\x04");
    p.wait_exit(15);
}

/// `--strict-config` refuses to start in TUI mode (both full PTY TUI and `TERM=dumb` plain TUI)
/// with exit code 2 when configuration warnings or config errors are present.
#[test]
fn strict_config_refuses_tui_startup_with_exit_code_2() {
    // 1. Unknown option warning in full TUI mode exits with code 2 before entering the main UI.
    let mut p = MareCmd::new()
        .faux(json!([]))
        .init_tail("mare.o.scrol_lines = 5\n")
        .arg("--strict-config")
        .no_term_probe()
        .pty(100, 30);
    let code = p.wait_exit_code(15);
    let out = p.text();
    assert_eq!(code, Some(2), "TUI --strict-config must exit with code 2; output: {out:?}");
    assert!(
        out.contains("--strict-config") && out.contains("scrol_lines"),
        "TUI --strict-config must report the config warning: {out:?}"
    );
    assert!(!out.contains("Ask mare anything"), "main UI must not start under --strict-config: {out:?}");

    // 2. Syntax/runtime error in init.lua under --strict-config exits immediately with code 2
    // without blocking on the interactive config error dialog.
    let mut p_err = MareCmd::new()
        .faux(json!([]))
        .init_tail("error('BROKEN strict init')\n")
        .arg("--strict-config")
        .no_term_probe()
        .pty(100, 30);
    let err_code = p_err.wait_exit_code(15);
    let err_out = p_err.text();
    assert_eq!(err_code, Some(2), "TUI --strict-config with broken init.lua must exit 2; output: {err_out:?}");
    assert!(err_out.contains("BROKEN strict init"), "broken init.lua message must be reported: {err_out:?}");
    assert!(!err_out.contains("Open in editor"), "interactive dialog must not block --strict-config: {err_out:?}");

    // 3. `TERM=dumb` plain interactive TUI path also honors --strict-config with exit code 2.
    let mut p_dumb = MareCmd::new()
        .faux(json!([]))
        .init_tail("mare.o.scrol_lines = 5\n")
        .arg("--strict-config")
        .env("TERM", "dumb")
        .no_term_probe()
        .pty(100, 30);
    let dumb_code = p_dumb.wait_exit_code(15);
    let dumb_out = p_dumb.text();
    assert_eq!(dumb_code, Some(2), "TERM=dumb --strict-config must exit with code 2; output: {dumb_out:?}");
    assert!(
        dumb_out.contains("--strict-config") && dumb_out.contains("scrol_lines"),
        "TERM=dumb --strict-config must report the config warning: {dumb_out:?}"
    );
}

/// `--profile-startup --strict-config` must still enforce strict configuration checks and exit
/// with code 2 even when a missing package triggers the first-frame install path.
#[test]
fn profile_startup_enforces_strict_config_even_when_installing_packages() {
    let pkg_repo = tempfile::tempdir().unwrap();
    let plugins_dir = pkg_repo.path().join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(plugins_dir.join("noop.lua"), "return { name = 'noop-pkg', apply = function() end }\n").unwrap();
    let git_ok = |args: &[&str]| {
        let out = fixture_git(pkg_repo.path()).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    };
    git_ok(&["init", "-b", "main"]);
    git_ok(&["config", "user.email", "test@example.com"]);
    git_ok(&["config", "user.name", "Test"]);
    git_ok(&["add", "."]);
    git_ok(&["commit", "-m", "init"]);

    let repo_url = format!("file://{}", pkg_repo.path().display());
    let init_tail = format!("mare.o.scrol_lines = 5\nmare.plugins.add {{ {repo_url:?} }}\n");
    let mut p = MareCmd::new()
        .faux(json!([]))
        .init_tail(&init_tail)
        .args(["--profile-startup", "--strict-config"])
        .no_term_probe()
        .pty(100, 30);
    let code = p.wait_exit_code(20);
    let out = p.text();
    assert_eq!(code, Some(2), "--profile-startup --strict-config must exit with code 2; output: {out:?}");
    assert!(
        out.contains("--strict-config") && out.contains("scrol_lines"),
        "--profile-startup --strict-config must report the config warning: {out:?}"
    );
}

/// Missing-package startup install screen must release boot_events so the main TUI still runs
/// terminal capability probing (OSC 11 / Kitty keyboard / CPR / DA1) and preserves focus state.
#[cfg(unix)]
#[test]
fn missing_package_install_screen_still_runs_terminal_probe_and_tracks_focus() {
    let pkg_repo = tempfile::tempdir().unwrap();
    let plugins_dir = pkg_repo.path().join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(plugins_dir.join("noop.lua"), "return { name = 'noop-pkg', apply = function() end }\n").unwrap();
    let git_ok = |args: &[&str]| {
        let out = fixture_git(pkg_repo.path()).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    };
    git_ok(&["init", "-b", "main"]);
    git_ok(&["config", "user.email", "test@example.com"]);
    git_ok(&["config", "user.name", "Test"]);
    git_ok(&["add", "."]);
    git_ok(&["commit", "-m", "init"]);

    let repo_url = format!("file://{}", pkg_repo.path().display());
    let init_tail = format!("mare.plugins.add {{ {repo_url:?} }}\n");
    // Do NOT set no_term_probe(): verify the post-first-frame probe still executes after install.
    let mut p = MareCmd::new().faux(json!([])).init_tail(&init_tail).pty(100, 30);
    // Wait for the probe's DA1 fence (`CSI c`), then reply with Kitty keyboard + CPR + DA1.
    p.wait_text("terminal capability probe query after install screen", |s| s.contains("\x1b[c"));
    p.send(b"\x1b[?1u\x1b[1;1R\x1b[?62;4c");
    p.wait_text("main UI with kitty keyboard enabled", |s| s.contains("Ask mare anything") && s.contains("\x1b[>5u"));
    p.send(b"\x04");
    p.wait_exit(15);
}

/// Resizing the terminal while the startup config-error dialog is open must propagate to the
/// main TUI when the user selects Continue.
#[cfg(unix)]
#[test]
fn config_error_dialog_resize_propagates_to_main_ui() {
    let mut p = start_pty(json!([]), false, "", "\nerror('BROKEN resize check')\n", &[]);
    p.wait_text("config error dialog", |s| s.contains("Config error") && s.contains("BROKEN resize check"));
    p.resize(64, 20);
    // Wait until the dialog itself has processed SIGWINCH and redrawn at width 64 before pressing `3`.
    p.wait_text("dialog redrawn at width 64", |s| s.contains(&format!("╰{}╯", "─".repeat(62))));
    // Select `3. Continue`.
    p.send(b"3");
    p.wait_text("main UI", |s| s.contains("Ask mare anything"));
    p.prompt(
        "/lua print('DIM=' .. mare.ui.width() .. 'x' .. mare.ui.height())",
        "updated dimensions after config error dialog",
        |s| s.contains("DIM=64x20"),
    );
    p.send(b"\x04");
    p.wait_exit(15);
}

/// Multiline bracketed paste and FocusOut buffered during the startup probe window must replay
/// as `Event::Paste` / `Event::FocusOut` (kept in the editor, never submitted line-by-line).
#[cfg(unix)]
#[test]
fn startup_probe_buffered_bracketed_paste_is_not_submitted_line_by_line() {
    let init_tail =
        "mare.on('ui/background', function() mare.ui.print('PROBE_FOCUSED=' .. tostring(mare.ui.focused())) end)\n";
    let mut p = MareCmd::new().faux(json!([{ "text": "UNEXPECTED_MODEL_REPLY" }])).init_tail(init_tail).pty(100, 30);
    p.wait_text("startup probe query", |s| s.contains("\x1b[c"));
    // Send OSC 11 + a multiline bracketed paste + FocusOut right before the DA1 fence.
    // `ui/background` runs right after the buffered events replay (before any keyboard input
    // arrives to infer focus), proving FocusOut was preserved from the startup probe.
    p.send(b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\\x1b[200~pasted alpha\rpasted beta\x1b[201~\x1b[O\x1b[?62;4c");
    p.wait_text("main UI with pasted content and preserved FocusOut", |s| {
        s.contains("pasted alpha") && s.contains("pasted beta") && s.contains("PROBE_FOCUSED=false")
    });
    // Give the host a brief moment to prove no turn was submitted.
    // sleep-ok: negative check verifying the pasted newline did not trigger an agent turn
    std::thread::sleep(Duration::from_millis(250));
    let out = p.text();
    assert!(
        !out.contains("UNEXPECTED_MODEL_REPLY"),
        "multiline paste during startup probe must not submit to the model: {}",
        tail(&out, 1200)
    );
    // Subsequent keyboard input clears the editor and proves focus back to true.
    p.send(b"\x03");
    p.wait_text("cleared editor", |s| s.contains("Ask mare anything"));
    p.send(b"/lua print('AFTER_KEY_FOCUSED=' .. tostring(mare.ui.focused()))\r");
    p.wait_text("keyboard input restores focus", |s| s.contains("AFTER_KEY_FOCUSED=true"));
    p.send(b"\x04");
    p.wait_exit(15);
}

/// Terminal capability replies that arrive AFTER the 250ms startup probe timeout (slow SSH/VPN)
/// must be routed in-band instead of dropped: enabling kitty keyboard, cursor anchor, and light theme.
#[cfg(unix)]
#[test]
fn late_terminal_probe_replies_after_timeout_are_applied_in_band() {
    let mut p = MareCmd::new().faux(json!([])).pty(100, 30);
    p.wait_text("startup probe query", |s| s.contains("\x1b[c"));
    // Let the 250ms startup probe time out so the main loop's tty::Events reader is active.
    p.wait_text("main UI ready after probe timeout", |s| s.contains("Ask mare anything"));
    // sleep-ok: ensure the 250ms read_probe budget has definitely elapsed and tty::Events owns stdin
    std::thread::sleep(Duration::from_millis(350));
    // Now deliver late replies: kitty keyboard flags, cursor position report, OSC 11 light bg, DA1.
    p.send(b"\x1b[?1u\x1b[12;1R\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?62;4c");
    p.wait_text("kitty keyboard enabled from late reply", |s| s.contains("\x1b[>5u"));
    p.send(
        b"/lua local c = mare.ui.caps(); print('LATE=' .. tostring(c.kitty_keyboard) .. ',' .. tostring(c.position_known))\r",
    );
    p.wait_text("caps updated from late replies", |s| s.contains("LATE=true,true"));
    p.send(b"\x04");
    p.wait_exit(15);
}

/// Pressing Ctrl+G must pause `mare-input` before handing off the tty so the external editor
/// receives every byte typed (including Enter) without mare stealing keystrokes or submitting the
/// edited prompt on return.
#[cfg(unix)]
#[test]
fn external_editor_ctrl_g_receives_all_keystrokes_without_stealing() {
    let dir = tempfile::tempdir().unwrap();
    let editor_script = dir.path().join("editor.sh");
    let log_path = dir.path().join("edlog");
    std::fs::write(
        &editor_script,
        "#!/bin/sh\nstty raw -echo\nprintf 'ED_READY'\ndd bs=1 count=7 2>/dev/null > \"$EDLOG\"\nstty sane\nprintf 'edited-no-steal\\n' > \"$1\"\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let editor = editor_script.to_string_lossy().into_owned();
    let log_env = log_path.to_string_lossy().into_owned();
    let mut p = start_pty(
        json!([{ "text": "UNEXPECTED_SUBMIT_REPLY" }]),
        false,
        "",
        "",
        &[("EDITOR", editor.clone()), ("VISUAL", editor), ("EDLOG", log_env)],
    );
    p.wait_text("the input box", |s| s.contains(INPUT_BOX));

    // Repeat 3 times in the same session to catch intermittent reader-thread races.
    for round in 0..3 {
        let _ = std::fs::remove_file(&log_path);
        let before_ready = p.text().matches("ED_READY").count();
        p.send(b"\x07"); // Ctrl+G
        p.wait_text("external editor ready", |s| s.matches("ED_READY").count() > before_ready);
        // Send 7 bytes (`abcdef\r`) one-by-one with short gaps so any live reader thread would steal one.
        for &b in b"abcdef\r" {
            p.send(&[b]);
            // sleep-ok: inter-keystroke pacing inside external editor to expose tty read contention
            std::thread::sleep(Duration::from_millis(40));
        }
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(got) = std::fs::read(&log_path)
                && got.len() == 7
            {
                assert_eq!(got, b"abcdef\r", "round {round}: external editor lost or corrupted keystrokes");
                break;
            }
            assert!(
                Instant::now() < end,
                "round {round}: external editor never received all 7 bytes (got={:?}); output: {:?}",
                std::fs::read(&log_path).ok(),
                tail(&p.text(), 800)
            );
            // sleep-ok: poll editor output file until all 7 bytes are written or deadline expires
            std::thread::sleep(Duration::from_millis(20));
        }
        p.wait_screen("edited text restored into mare editor", |s| s.contains("edited-no-steal"));
    }

    // Confirm the trailing `\r` sent to the editor was never stolen to submit a prompt.
    // sleep-ok: negative check confirming no prompt submission occurred after external editor exit
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        !p.text().contains("UNEXPECTED_SUBMIT_REPLY"),
        "Enter typed inside external editor leaked into mare and submitted the prompt: {}",
        tail(&p.text(), 1000)
    );
    // Clear editor and quit cleanly.
    p.send(b"\x03");
    p.wait_screen("editor cleared", |s| !s.contains("edited-no-steal"));
    p.send(b"\x04");
    p.wait_exit(15);
}

/// Pressing Ctrl+C while a cooked-mode external editor (e.g. `code --wait`) owns the terminal
/// must interrupt the editor and return to mare instead of exiting mare with SIGINT.
#[cfg(unix)]
#[test]
fn cooked_external_editor_sigint_cancels_editor_without_exiting_mare() {
    let dir = tempfile::tempdir().unwrap();
    let editor_script = dir.path().join("cooked-editor.sh");
    std::fs::write(&editor_script, "#!/bin/sh\nprintf 'COOKED_ED_READY'\nexec sleep 60\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let editor = editor_script.to_string_lossy().into_owned();
    let mut p = start_pty(json!([]), false, "", "", &[("EDITOR", editor.clone()), ("VISUAL", editor)]);
    p.wait_text("the input box", |s| s.contains(INPUT_BOX));

    p.send(b"\x07"); // Ctrl+G
    p.wait_text("cooked external editor ready", |s| s.contains("COOKED_ED_READY"));
    // Ctrl+C in cooked mode delivers SIGINT to the foreground process group (editor + mare).
    p.send(b"\x03");
    p.wait_screen("warning notice after interrupted editor", |s| s.contains("exited with"));
    assert!(p.running(), "Ctrl+C during cooked-mode external editor must not exit mare");

    // Verify mare remains responsive and interactive after editor SIGINT.
    p.send(b"/lua print('ALIVE_AFTER_COOKED_SIGINT')\r");
    p.wait_text("mare stays interactive after editor SIGINT", |s| s.contains("ALIVE_AFTER_COOKED_SIGINT"));
    p.send(b"\x04");
    p.wait_exit(15);
}

/// Receiving `SIGTSTP` (e.g. Ctrl+Z in a job-control shell) while a cooked-mode external editor
/// owns the terminal must suspend mare alongside the editor instead of hanging the foreground job,
/// and `SIGCONT` (`fg`) must resume waiting on the editor until it exits and returns the draft.
#[cfg(unix)]
#[test]
fn cooked_external_editor_sigtstp_suspends_and_resumes_editor_cleanly() {
    use std::fs::File;
    use std::io::{Read as _, Write as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let release_file = dir.path().join("release");
    let editor_script = dir.path().join("cooked-tstp-editor.sh");
    std::fs::write(
        &editor_script,
        format!(
            "#!/bin/sh\nprintf 'COOKED_TSTP_READY'\nwhile [ ! -f \"{}\" ]; do sleep 0.02; done\nprintf 'resumed-editor-draft\\n' > \"$1\"\n",
            release_file.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&editor_script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize { ws_row: 30, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: openpty initializes both descriptors from a valid winsize pointer.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(size),
            )
        },
        0
    );
    // SAFETY: openpty returned two owned descriptors.
    let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        // SAFETY: fd is valid; mark close-on-exec so only duplicated stdio fds cross exec.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }, 0);
    }

    let editor = editor_script.to_string_lossy().into_owned();
    let mut cmd = crate::common::mare_cmd();
    for (k, _) in std::env::vars_os() {
        if (k.to_string_lossy().starts_with("MARE_") && k != "MARE_TEST_OFFLINE")
            || matches!(k.to_str(), Some("GIT_DIR" | "GIT_WORK_TREE" | "GIT_INDEX_FILE"))
        {
            cmd.env_remove(k);
        }
    }
    cmd.args(["--model", "faux/echo", "--no-session", "--no-trust", "--set", "plugins.onboarding.plugins=false"])
        .current_dir(&work)
        .env("MARE_HOME", &home)
        .env("MARE_NO_TERM_PROBE", "1")
        .env("TERM", "xterm-256color")
        .env("EDITOR", &editor)
        .env("VISUAL", &editor)
        // Non-orphaned child process group (like a job-control shell) so SIG_DFL SIGTSTP stops it.
        .process_group(0)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    // SAFETY: resetting SIGTSTP to SIG_DFL is async-signal-safe in pre_exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::signal(libc::SIGTSTP, libc::SIG_DFL) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = ChildGuard(cmd.spawn().unwrap());
    drop(cmd);
    let mare_pid = child.0.id() as libc::pid_t;

    let read_some = |master: &mut File, out: &mut Vec<u8>| {
        let mut pfd = libc::pollfd { fd: master.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: pfd points to one valid descriptor.
        if unsafe { libc::poll(&mut pfd, 1, 25) } > 0 && (pfd.revents & libc::POLLIN) != 0 {
            let mut buf = [0u8; 8192];
            if let Ok(n) = master.read(&mut buf) {
                out.extend_from_slice(&buf[..n]);
            }
        }
    };

    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !String::from_utf8_lossy(&out).contains(INPUT_BOX) {
        assert!(Instant::now() < deadline, "timed out waiting for input box: {}", String::from_utf8_lossy(&out));
        read_some(&mut master, &mut out);
    }

    // Open cooked external editor via Ctrl+G.
    master.write_all(b"\x07").unwrap();
    master.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !String::from_utf8_lossy(&out).contains("COOKED_TSTP_READY") {
        assert!(Instant::now() < deadline, "timed out waiting for editor ready: {}", String::from_utf8_lossy(&out));
        read_some(&mut master, &mut out);
    }

    // Deliver SIGTSTP to the foreground process group (mare + cooked editor), as Ctrl+Z does.
    // SAFETY: mare_pid is the live process group leader created by `process_group(0)`.
    assert_eq!(unsafe { libc::kill(-mare_pid, libc::SIGTSTP) }, 0);
    let stop_deadline = Instant::now() + Duration::from_secs(10);
    let mut stopped = false;
    while Instant::now() < stop_deadline {
        let mut status: libc::c_int = 0;
        // SAFETY: mare_pid is our direct child process.
        let rc = unsafe { libc::waitpid(mare_pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
        if rc == mare_pid && libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGTSTP {
            stopped = true;
            break;
        }
        read_some(&mut master, &mut out);
    }
    assert!(stopped, "mare must stop on SIGTSTP while cooked external editor is open");

    // Resume the whole process group (as `fg` does) and let the editor save and exit.
    // SAFETY: -mare_pid is our stopped child process group.
    assert_eq!(unsafe { libc::kill(-mare_pid, libc::SIGCONT) }, 0);
    std::fs::write(&release_file, "go").unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    while !String::from_utf8_lossy(&out).contains("resumed-editor-draft") {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for edited draft after SIGCONT: {}",
            String::from_utf8_lossy(&out)
        );
        read_some(&mut master, &mut out);
    }

    // Clear draft and exit cleanly.
    master.write_all(b"\x03\x04").unwrap();
    master.flush().unwrap();
    let exit_deadline = Instant::now() + Duration::from_secs(15);
    let mut exited = false;
    while Instant::now() < exit_deadline {
        if child.0.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        read_some(&mut master, &mut out);
    }
    assert!(exited, "mare did not exit cleanly after resuming from editor SIGTSTP");
}

/// Receiving SIGTERM while a raw-mode, alt-screen external editor is open must terminate the
/// editor, exit its alt screen, and restore cooked terminal modes (`icanon`, `echo`) on exit.
#[cfg(unix)]
#[test]
fn sigterm_while_raw_external_editor_is_open_restores_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let editor_script = dir.path().join("raw-alt-editor.sh");
    let mare_pid_file = dir.path().join("mare.pid");
    // Enter raw mode + alt screen and ignore SIGTERM so mare must escalate to SIGKILL and restore
    // the terminal itself (worst-case stuck raw-mode editor).
    std::fs::write(
        &editor_script,
        "#!/bin/sh\necho \"$PPID\" > \"$MAREPID\"\nstty raw -echo\nprintf '\\033[?1049hRAW_ALT_READY'\nexec python3 -c 'import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(30)'\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let editor = editor_script.to_string_lossy().into_owned();
    let mare_pid_env = mare_pid_file.to_string_lossy().into_owned();
    let mut p = start_pty(
        json!([]),
        true,
        "",
        "",
        &[("EDITOR", editor.clone()), ("VISUAL", editor), ("MAREPID", mare_pid_env)],
    );
    p.wait_text("the input box", |s| s.contains(INPUT_BOX));

    p.send(b"\x07"); // Ctrl+G
    p.wait_text("raw alt-screen editor ready", |s| s.contains("RAW_ALT_READY"));
    let mare_pid: libc::pid_t =
        std::fs::read_to_string(&mare_pid_file).expect("editor wrote mare pid").trim().parse().expect("valid mare pid");
    // SAFETY: mare_pid is the live child mare process spawned inside our PTY test.
    assert_eq!(unsafe { libc::kill(mare_pid, libc::SIGTERM) }, 0);
    p.wait_exit(15);

    let end = Instant::now() + Duration::from_secs(WAIT_SECS);
    while !stty_file(p.home_path()).exists() && Instant::now() < end {
        // sleep-ok: polls for the stty file written by the wrapper shell after mare exits
        std::thread::sleep(Duration::from_millis(20));
    }
    let stty = std::fs::read_to_string(stty_file(p.home_path())).unwrap_or_default();
    let flags: Vec<&str> = stty.split_whitespace().collect();
    assert!(
        flags.contains(&"icanon") && flags.contains(&"echo"),
        "tty left in raw mode after SIGTERM during raw external editor: {stty}"
    );
    let out = p.text();
    let after_ready = out.split("RAW_ALT_READY").nth(1).unwrap_or("");
    assert!(
        after_ready.contains("\x1b[?1049l") && after_ready.contains("\x1b[?25h"),
        "terminal must leave alt screen and show cursor when exiting during raw editor: {after_ready:?}"
    );
}

/// Rapid single-write PTY bursts must never transpose characters across `mare-input` and the
/// main event loop, including slash commands and `/exit`.
#[cfg(unix)]
#[test]
fn rapid_pty_burst_input_preserves_character_order() {
    let script = json!([
        { "text": "burst-reply-1" },
        { "text": "burst-reply-2" },
        { "text": "burst-reply-3" },
        { "text": "burst-reply-4" }
    ]);
    let mut p = start_pty(script, false, "", "", &[]);
    p.wait_text("the input box", |s| s.contains(INPUT_BOX));
    // Ensure startup has settled before sending raw single-write bursts (`INPUT_BOX` renders on
    // the first frame while `app.startup.starting == true` and Enter is still ignored).
    p.prompt("/lua print('BURST_' .. 'READY')", "startup settled", |s| s.contains("BURST_READY"));

    for (idx, prompt) in
        ["first pty burst prompt", "second pty burst prompt", "third pty burst prompt", "fourth pty burst prompt"]
            .iter()
            .enumerate()
    {
        let marker = format!("BURST_OK_{idx}");
        let cmd = format!("/lua print('BURST_' .. 'OK_' .. {idx})\r");
        p.send(cmd.as_bytes());
        p.wait_text("slash command burst echoed in order", |s| s.contains(&marker));

        let line = format!("{prompt}\r");
        let reply = format!("burst-reply-{}", idx + 1);
        p.send(line.as_bytes());
        p.wait_screen("prompt burst answered and settled", |s| s.contains(&reply) && s.contains("Ask mare anything"));
        let rendered = screen(&p.text());
        assert!(
            rendered.contains(prompt),
            "burst prompt {idx} was corrupted on screen (expected {prompt:?}):\n{rendered}"
        );
    }

    p.send(b"/exit\r");
    p.wait_exit(15);
}

/// Issue #176: after Ctrl+Z (SIGTSTP) and resuming in the foreground (SIGCONT), Mare should
/// re-query the terminal background (OSC 11) so `flavor = "auto"` updates when the terminal
/// switched between dark and light while suspended.
#[cfg(unix)]
#[test]
fn resume_from_sigtstp_requeries_terminal_background() {
    use std::fs::File;
    use std::io::{Read as _, Write as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    let bg_log = dir.path().join("bg.log");
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        home.join("config/init.lua"),
        r#"
local log_path = os.getenv("MARE_REPRO_BG_LOG")
local function append(line)
    local f = io.open(log_path, "a")
    if f then
        f:write(line .. "\n")
        f:close()
    end
end
mare.on("ui/background", function(ev)
    append(string.format("bg:%s:%d,%d,%d", tostring(ev.dark), ev.r, ev.g, ev.b))
end)
mare.on("ui/resume", function()
    append("resumed")
end)
"#,
    )
    .unwrap();

    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize { ws_row: 30, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: openpty initializes both descriptors from a valid winsize pointer.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(size),
            )
        },
        0
    );
    // SAFETY: openpty returned two owned descriptors.
    let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        // SAFETY: fd is valid; mark close-on-exec so only duplicated stdio fds cross exec.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }, 0);
    }

    let mut cmd = crate::common::mare_cmd();
    for (k, _) in std::env::vars_os() {
        if (k.to_string_lossy().starts_with("MARE_") && k != "MARE_TEST_OFFLINE")
            || matches!(k.to_str(), Some("GIT_DIR" | "GIT_WORK_TREE" | "GIT_INDEX_FILE"))
        {
            cmd.env_remove(k);
        }
    }
    cmd.args([
        "--model",
        "faux/echo",
        "--no-session",
        "--no-trust",
        "--set",
        "plugins.onboarding.plugins=false",
        "--set",
        "flavor=auto",
    ])
    .current_dir(&work)
    .env("MARE_HOME", &home)
    .env("MARE_REPRO_BG_LOG", &bg_log)
    .env("TERM", "xterm-256color")
    .process_group(0)
    .stdin(Stdio::from(slave.try_clone().unwrap()))
    .stdout(Stdio::from(slave.try_clone().unwrap()))
    .stderr(Stdio::from(slave.try_clone().unwrap()));
    // SAFETY: resetting SIGTSTP to SIG_DFL is async-signal-safe in pre_exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::signal(libc::SIGTSTP, libc::SIG_DFL) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child = ChildGuard(cmd.spawn().unwrap());
    drop(cmd);
    let mare_pid = child.0.id() as libc::pid_t;

    let read_some = |master: &mut File, out: &mut Vec<u8>| {
        let mut pfd = libc::pollfd { fd: master.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: pfd points to one valid descriptor.
        if unsafe { libc::poll(&mut pfd, 1, 25) } > 0 && (pfd.revents & libc::POLLIN) != 0 {
            let mut buf = [0u8; 8192];
            if let Ok(n) = master.read(&mut buf) {
                out.extend_from_slice(&buf[..n]);
            }
        }
    };

    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !String::from_utf8_lossy(&out).contains("\x1b]11;?") {
        assert!(Instant::now() < deadline, "timed out waiting for startup OSC 11 query");
        read_some(&mut master, &mut out);
    }

    // Answer startup probe with a dark terminal background and DA1 fence.
    master.write_all(b"\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[?62;22c").unwrap();
    master.flush().unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    while !String::from_utf8_lossy(&out).contains(INPUT_BOX)
        || !std::fs::read_to_string(&bg_log).unwrap_or_default().contains("bg:true:0,0,0")
    {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for startup dark theme; log={:?}",
            std::fs::read_to_string(&bg_log).ok()
        );
        read_some(&mut master, &mut out);
    }

    // Suspend mare with Ctrl+Z.
    let before_suspend = out.len();
    master.write_all(b"\x1a").unwrap();
    master.flush().unwrap();
    let stop_deadline = Instant::now() + Duration::from_secs(10);
    let mut stopped = false;
    while Instant::now() < stop_deadline {
        let mut status: libc::c_int = 0;
        // SAFETY: mare_pid is our direct child process.
        let rc = unsafe { libc::waitpid(mare_pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
        if rc == mare_pid && libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGTSTP {
            stopped = true;
            break;
        }
        read_some(&mut master, &mut out);
    }
    assert!(stopped, "mare did not stop on Ctrl+Z");

    // Resume mare in the foreground (`fg` / SIGCONT) after the terminal switched to light mode.
    // SAFETY: -mare_pid is our stopped child process group.
    assert_eq!(unsafe { libc::kill(-mare_pid, libc::SIGCONT) }, 0);
    let resume_deadline = Instant::now() + Duration::from_secs(15);
    while !std::fs::read_to_string(&bg_log).unwrap_or_default().contains("resumed") {
        assert!(
            Instant::now() < resume_deadline,
            "timed out waiting for ui/resume; log={:?}",
            std::fs::read_to_string(&bg_log).ok()
        );
        read_some(&mut master, &mut out);
    }
    // Also deliver FocusIn in case focus-triggered probing would fire.
    master.write_all(b"\x1b[I").unwrap();
    master.flush().unwrap();

    // Wait briefly for a post-resume OSC 11 query; if one arrives, reply with a light background.
    let probe_deadline = Instant::now() + Duration::from_millis(600);
    let mut saw_resume_osc11 = false;
    while Instant::now() < probe_deadline {
        read_some(&mut master, &mut out);
        if String::from_utf8_lossy(&out[before_suspend..]).contains("\x1b]11;?") {
            saw_resume_osc11 = true;
            master.write_all(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\").unwrap();
            master.flush().unwrap();
            break;
        }
    }
    if saw_resume_osc11 {
        let light_deadline = Instant::now() + Duration::from_secs(5);
        while !std::fs::read_to_string(&bg_log).unwrap_or_default().contains("bg:false:255,255,255")
            && Instant::now() < light_deadline
        {
            read_some(&mut master, &mut out);
        }
    }

    let log_contents = std::fs::read_to_string(&bg_log).unwrap_or_default();
    assert!(
        saw_resume_osc11 && log_contents.contains("bg:false:255,255,255"),
        "expected OSC 11 re-query and light ui/background after resume; saw_resume_osc11={saw_resume_osc11}, log={log_contents:?}"
    );
}

/// Issue #176: explicit `flavor=dark` must suppress post-resume OSC 11 queries.
#[cfg(unix)]
#[test]
fn resume_from_sigtstp_skips_osc11_when_flavor_is_explicit() {
    use std::fs::File;
    use std::io::{Read as _, Write as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    let bg_log = dir.path().join("bg.log");
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        home.join("config/init.lua"),
        r#"
local log_path = os.getenv("MARE_REPRO_BG_LOG")
mare.on("ui/resume", function()
    local f = io.open(log_path, "a")
    if f then f:write("resumed\n"); f:close() end
end)
"#,
    )
    .unwrap();

    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize { ws_row: 30, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(size),
            )
        },
        0
    );
    let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }, 0);
    }

    let mut cmd = crate::common::mare_cmd();
    for (k, _) in std::env::vars_os() {
        if (k.to_string_lossy().starts_with("MARE_") && k != "MARE_TEST_OFFLINE")
            || matches!(k.to_str(), Some("GIT_DIR" | "GIT_WORK_TREE" | "GIT_INDEX_FILE"))
        {
            cmd.env_remove(k);
        }
    }
    cmd.args([
        "--model",
        "faux/echo",
        "--no-session",
        "--no-trust",
        "--set",
        "plugins.onboarding.plugins=false",
        "--set",
        "flavor=dark",
    ])
    .current_dir(&work)
    .env("MARE_HOME", &home)
    .env("MARE_REPRO_BG_LOG", &bg_log)
    .env("TERM", "xterm-256color")
    .process_group(0)
    .stdin(Stdio::from(slave.try_clone().unwrap()))
    .stdout(Stdio::from(slave.try_clone().unwrap()))
    .stderr(Stdio::from(slave.try_clone().unwrap()));
    unsafe {
        cmd.pre_exec(|| {
            if libc::signal(libc::SIGTSTP, libc::SIG_DFL) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child = ChildGuard(cmd.spawn().unwrap());
    drop(cmd);
    let mare_pid = child.0.id() as libc::pid_t;

    let read_some = |master: &mut File, out: &mut Vec<u8>| {
        let mut pfd = libc::pollfd { fd: master.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, 25) } > 0 && (pfd.revents & libc::POLLIN) != 0 {
            let mut buf = [0u8; 8192];
            if let Ok(n) = master.read(&mut buf) {
                out.extend_from_slice(&buf[..n]);
            }
        }
    };

    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !String::from_utf8_lossy(&out).contains("\x1b[c") {
        assert!(Instant::now() < deadline, "timed out waiting for startup DA1 query");
        read_some(&mut master, &mut out);
    }
    master.write_all(b"\x1b[?62;22c").unwrap();
    master.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !String::from_utf8_lossy(&out).contains(INPUT_BOX) {
        assert!(Instant::now() < deadline, "timed out waiting for input box");
        read_some(&mut master, &mut out);
    }

    let before_suspend = out.len();
    master.write_all(b"\x1a").unwrap();
    master.flush().unwrap();
    let stop_deadline = Instant::now() + Duration::from_secs(10);
    let mut stopped = false;
    while Instant::now() < stop_deadline {
        let mut status: libc::c_int = 0;
        let rc = unsafe { libc::waitpid(mare_pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
        if rc == mare_pid && libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGTSTP {
            stopped = true;
            break;
        }
        read_some(&mut master, &mut out);
    }
    assert!(stopped, "mare did not stop on Ctrl+Z");

    assert_eq!(unsafe { libc::kill(-mare_pid, libc::SIGCONT) }, 0);
    let resume_deadline = Instant::now() + Duration::from_secs(15);
    while !std::fs::read_to_string(&bg_log).unwrap_or_default().contains("resumed") {
        assert!(Instant::now() < resume_deadline, "timed out waiting for ui/resume");
        read_some(&mut master, &mut out);
    }
    let drain_deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < drain_deadline {
        read_some(&mut master, &mut out);
    }
    assert!(
        !String::from_utf8_lossy(&out[before_suspend..]).contains("\x1b]11;?"),
        "explicit flavor=dark must not emit OSC 11 on resume"
    );
}
