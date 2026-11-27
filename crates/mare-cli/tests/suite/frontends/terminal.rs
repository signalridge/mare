//! Real process regressions for terminal.
#[cfg(unix)]
use std::{
    path::Path,
    time::{Duration, Instant},
};

use serde_json::Value;
#[cfg(unix)]
use serde_json::json;

use super::*;

#[cfg(unix)]
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(unix)]
fn wait_theme_snapshot(p: &PtySession, result: &Path, what: &str, pred: impl Fn(&Value, &str) -> bool) -> Value {
    let end = Instant::now() + Duration::from_secs(WAIT_SECS);
    loop {
        let val = std::fs::read_to_string(result)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or(Value::Null);
        let txt = p.text();
        if pred(&val, &txt) {
            return val;
        }
        assert!(
            Instant::now() < end,
            "timed out waiting for {what}; snapshot={val}, output tail: {:?}",
            tail(&txt, 1000)
        );
        std::thread::sleep(Duration::from_millis(20)); // sleep-ok: poll until theme probe predicate matches
    }
}

#[cfg(unix)]
const THEME_PROBE_LUA: &str = r##"
return {
  name = "theme-probe",
  inject = { "theme", "ui" },
  apply = function(ctx)
    local theme = ctx:get("theme")
    theme:register({ name="aaa-preview", base="dark", palette={accent="#123456", green="#123456"} })
    ctx.ui:widget("code-sample", {order=-1000, render=function()
      return ui.Markdown("```rust\nfn theme_sample() {}\n```\n```diff\n+syntax addition\n```")
    end})
    local ended = 0
    local function snapshot()
      local value = theme:get()
      value.ended = ended
      assert(mare.fs.write(mare.env("THEME_PTY_RESULT"), mare.json.encode(value)))
    end
    ctx:on("theme/changed", snapshot)
    ctx:on("app/ready", snapshot)
    ctx:on("turn/completed", function() ended = ended + 1; snapshot() end)
  end,
}
"##;

#[cfg(unix)]
#[test]
fn theme_pty_preview_cancel_and_persistence() {
    let home = tempfile::tempdir().unwrap();
    let result = home.path().join("theme.json");
    let script = json!([
        { "text": "```rust\nfn history_syntax() {}\n```\nseed complete" },
        { "text": "theme faux complete" }
    ]);

    let mut p = MareCmd::new()
        .home_dir(home.path())
        .arg("--no-session")
        .faux(script.clone())
        .plugin("theme-probe", THEME_PROBE_LUA)
        .env("COLORTERM", "truecolor")
        .env_remove("NO_COLOR")
        .env("THEME_PTY_RESULT", &result)
        .pty(100, 40);

    wait_theme_snapshot(&p, &result, "initial code render", |v, txt| {
        v["preference"] == "auto" && txt.contains("theme_sample")
    });

    p.send(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\");
    let baseline = wait_theme_snapshot(&p, &result, "actual OSC11 light background", |v, _| {
        v["appearance"] == "light" && v["active"] == "light"
    });

    p.prompt("seed history", "committed syntax history", |s| s.contains("history_syntax"));
    wait_theme_snapshot(&p, &result, "agent end after seed", |v, _| v["ended"].as_u64().unwrap_or(0) >= 1);

    p.send(b"/theme");
    p.wait_screen("/theme", |s| s.contains("/theme"));
    p.send(b"\r");
    wait_theme_snapshot(&p, &result, "picker opened", |v, _| v["preview"] == "auto");

    let before_preview = p.bytes().len();
    p.send(b"\x1b[B");
    wait_theme_snapshot(&p, &result, "highlight preview", |v, _| v["preview"] == "aaa-preview");
    p.wait_raw("existing syntax history replayed", |raw| {
        raw.len() > before_preview
            && find_subslice(&raw[before_preview..], b"\x1b[3J").is_some()
            && find_subslice(&raw[before_preview..], b"history_syntax").is_some()
    });
    p.wait_text("preview actual RGB", |s| s.contains("38;2;18;52;86"));

    p.send(b"\x1b");
    let restored = wait_theme_snapshot(&p, &result, "escape restore", |v, _| {
        v["preview"].is_null() && v["active"] == baseline["active"]
    });
    assert_eq!(restored["preference"], "auto");

    p.send(b"/theme");
    p.wait_screen("/theme", |s| s.contains("/theme"));
    p.send(b"\r");
    wait_theme_snapshot(&p, &result, "picker reopened", |v, _| v["preview"] == "auto");
    p.send(b"\x1b[B");
    wait_theme_snapshot(&p, &result, "second preview", |v, _| v["preview"] == "aaa-preview");
    p.send(b"\r");
    wait_theme_snapshot(&p, &result, "confirmed saved selection", |v, _| {
        v["preview"].is_null() && v["preference"] == "aaa-preview"
    });

    p.prompt("check theme", "faux after selection", |s| s.contains("theme faux complete"));
    p.send(b"\x04");
    p.wait_exit(15);

    let _ = std::fs::remove_file(&result);
    let mut restarted = MareCmd::new()
        .home_dir(home.path())
        .no_init()
        .arg("--no-session")
        .env("COLORTERM", "truecolor")
        .env_remove("NO_COLOR")
        .env("THEME_PTY_RESULT", &result)
        .pty(100, 40);

    wait_theme_snapshot(&restarted, &result, "restart persisted theme", |v, _| {
        v["preference"] == "aaa-preview" && v["active"] == "aaa-preview"
    });
    restarted.send(b"\x04");
    restarted.wait_exit(15);
}

/// Kitty keyboard query-first with one pop, zero bytes while idle, hostile tool output filtered
/// .
#[cfg(unix)]
#[test]
fn terminal_kitty_idle_and_hostile_output_under_a_pty() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const INPUT_BOX: &[u8] = "╰".as_bytes();
    const BOOT: Duration = Duration::from_secs(100);
    const NOTICE_LIFE: Duration = Duration::from_secs(6);
    const KITTY_TRIES: usize = 6;

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        find_subslice(haystack, needle).is_some()
    }
    fn count_subseq(haystack: &[u8], needle: &[u8]) -> usize {
        haystack.windows(needle.len()).filter(|w| *w == needle).count()
    }
    fn plain(raw: &[u8]) -> String {
        let s = String::from_utf8_lossy(raw);
        let re = regex::Regex::new(r"\x1b\[[0-9;?<>]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").unwrap();
        re.replace_all(&s, "").into_owned()
    }
    fn settle(p: &PtySession, quiet: Duration, ceiling: Duration) {
        let end = Instant::now() + ceiling;
        let mut last_len = p.bytes().len();
        let mut last_change = Instant::now();
        while Instant::now() < end && last_change.elapsed() < quiet {
            // sleep-ok: bounded poll waiting for PTY output to settle
            std::thread::sleep(Duration::from_millis(25));
            let cur_len = p.bytes().len();
            if cur_len != last_len {
                last_len = cur_len;
                last_change = Instant::now();
            }
        }
    }
    fn quit_tui(p: &mut PtySession) -> Vec<u8> {
        let mark = p.bytes().len();
        p.send(b"\x04");
        let step_end = Instant::now() + Duration::from_secs(2);
        while p.try_exit().is_none() && Instant::now() < step_end {
            // sleep-ok: bounded poll waiting for Ctrl-D exit before falling back to Ctrl-C
            std::thread::sleep(Duration::from_millis(25));
        }
        let deadline = Instant::now() + BOOT;
        while p.try_exit().is_none() {
            assert!(Instant::now() < deadline, "timed out waiting for TUI PTY child {} to quit", p.pid());
            p.send(b"\x03");
            // sleep-ok: bounded Ctrl-C retry interval during TUI shutdown
            std::thread::sleep(Duration::from_millis(500));
        }
        settle(p, Duration::from_millis(200), Duration::from_secs(1));
        p.bytes()[mark..].to_vec()
    }
    fn spawn_tty(args: &[&str], answer_kitty: bool, env_extra: &[(&str, &str)], init_tail: Option<&str>) -> PtySession {
        let answered = Arc::new(AtomicBool::new(false));
        let mut cmd = MareCmd::new()
            .arg("--model")
            .arg("faux/echo")
            .args(args.iter().copied())
            .env("TERM", "xterm-256color")
            .env_remove("TMUX")
            .respond(move |buf| {
                if !answered.load(Ordering::Relaxed) && contains(buf, b"\x1b[6n") {
                    answered.store(true, Ordering::Relaxed);
                    let reply: &[u8] =
                        if answer_kitty { b"\x1b[?1u\x1b[3;1R\x1b[?62;22c" } else { b"\x1b[3;1R\x1b[?62;22c" };
                    Some(reply.to_vec())
                } else {
                    None
                }
            });
        for (k, v) in env_extra {
            cmd = cmd.env(k, v);
        }
        if let Some(lua) = init_tail {
            cmd = cmd.init_tail(lua);
        } else {
            cmd = cmd.no_init();
        }
        cmd.pty(100, 30)
    }

    let kitty_query_first = || {
        for answer in [true, false] {
            let mut proc_opt = None;
            for attempt in 0..KITTY_TRIES {
                let mut p = spawn_tty(&[], answer, &[], None);
                p.wait_raw("input box", |b| contains(b, INPUT_BOX));
                if !answer {
                    proc_opt = Some(p);
                    break;
                }
                let end = Instant::now() + Duration::from_secs(10);
                while !contains(&p.bytes(), b"\x1b[>5u") && Instant::now() < end {
                    // sleep-ok: bounded poll waiting for kitty push after probe answer
                    std::thread::sleep(Duration::from_millis(25));
                }
                if contains(&p.bytes(), b"\x1b[>5u") || attempt + 1 == KITTY_TRIES {
                    proc_opt = Some(p);
                    break;
                }
                quit_tui(&mut p);
            }
            let mut p = proc_opt.unwrap();
            settle(&p, Duration::from_secs(1), BOOT);
            let before_quit = p.bytes();
            let tail_bytes = quit_tui(&mut p);
            let q = find_subslice(&before_quit, b"\x1b[6n");
            if answer {
                let push = find_subslice(&before_quit, b"\x1b[>5u");
                assert!(
                    push.is_some() && q.is_some() && push.unwrap() > q.unwrap(),
                    "kitty must be pushed after probe"
                );
                assert_eq!(count_subseq(&before_quit, b"\x1b[>5u"), 1, "pushed once");
                assert_eq!(count_subseq(&tail_bytes, b"\x1b[<1u"), 1, "popped exactly once");
            } else {
                assert!(!contains(&before_quit, b"\x1b[>5u"), "no answer: never push");
                assert!(!contains(&tail_bytes, b"\x1b[<"), "never pop an entry we didn't push");
            }
        }
    };

    let idle_writes_nothing = || {
        let mut p = spawn_tty(&[], false, &[], None);
        p.wait_raw("input box", |b| contains(b, INPUT_BOX));
        p.wait_raw("Permissions card", |b| contains(b, b"Permissions"));
        // sleep-ok: wait out 6s startup permissions notice lifetime before checking idle
        std::thread::sleep(NOTICE_LIFE + Duration::from_secs(1));
        settle(&p, Duration::from_secs(3), BOOT);
        let mark = p.bytes().len();
        // sleep-ok: 2s idle window to verify zero bytes written by idle TUI
        std::thread::sleep(Duration::from_secs(2));
        let idle = p.bytes()[mark..].to_vec();
        quit_tui(&mut p);
        assert!(idle.is_empty(), "an idle TUI wrote {} bytes: {:?}", idle.len(), &idle[..idle.len().min(200)]);
    };

    let resize_redraws_at_the_new_width = || {
        let mut p = spawn_tty(&[], false, &[], None);
        p.wait_raw("input box", |b| contains(b, INPUT_BOX));
        p.wait_raw("Permissions card", |b| contains(b, b"Permissions"));
        // sleep-ok: wait out 6s startup permissions notice lifetime before resize
        std::thread::sleep(NOTICE_LIFE + Duration::from_secs(1));
        settle(&p, Duration::from_secs(3), BOOT);
        let mark = p.bytes().len();
        p.resize(80, 30);
        let box_re = regex::Regex::new("╰─+╯").unwrap();
        p.wait_raw("resized input box", |b| box_re.is_match(&String::from_utf8_lossy(&b[mark..])));
        settle(&p, Duration::from_millis(500), BOOT);
        let after = String::from_utf8_lossy(&p.bytes()[mark..]).into_owned();
        assert!(p.running(), "mare exited on a resize: {after}");
        let widths: Vec<usize> = box_re.find_iter(&after).map(|m| m.as_str().chars().count()).collect();
        assert!(!widths.is_empty() && *widths.iter().max().unwrap() <= 80, "input box not redrawn <= 80: {widths:?}");
        quit_tui(&mut p);
    };

    let hostile_output_is_filtered = || {
        let hostile = r"printf '\033]52;c;ZXZpbA==\007\033]0;pwned\007\033[2J\033[1;1HVISIBLE-TEXT\n'";
        let mut p = MareCmd::new()
            .no_init()
            .arg("--model")
            .arg("faux/echo")
            .arg("--yolo")
            .faux(json!([
                { "tool_calls": [{ "name": "bash", "arguments": { "command": hostile } }] },
                { "text": "all done" },
            ]))
            .env("TERM", "xterm-256color")
            .env_remove("TMUX")
            .pty(100, 30);
        p.wait_raw("input box", |b| contains(b, INPUT_BOX));
        settle(&p, Duration::from_millis(500), BOOT);
        p.prompt("go", "all done", |t| t.contains("all done"));
        quit_tui(&mut p);
        let out = p.bytes();
        assert!(contains(&out, b"all done"), "turn didn't finish: {}", plain(&out));
        assert!(contains(&out, b"VISIBLE-TEXT"), "the text itself is still shown");
        assert!(!contains(&out, b"\x1b]52"), "clipboard write reached the terminal");
        assert!(!contains(&out, b"\x1b]0;pwned") && !contains(&out, b"\x1b]2;pwned"), "title changed");
        assert!(!contains(&out, b"\x1b[2J"), "screen cleared by tool output");
    };

    let extension_points = || {
        let answered = Arc::new(AtomicBool::new(false));
        let mut p = MareCmd::new()
            .arg("--model")
            .arg("faux/echo")
            .setup("logo = true")
            .init_tail(
                "mare.on(\"ui/commit-before\", function(e, next) e.lines[#e.lines + 1] = \"HOOKED-LINE\"; return next(e) end)\n\
                 mare.on(\"ui/caps\", function(c, next) c.sync = false; return next(c) end)\n",
            )
            .env("TERM", "xterm-256color")
            .env_remove("TMUX")
            .respond(move |buf| {
                if !answered.load(Ordering::Relaxed) && contains(buf, b"\x1b[6n") {
                    answered.store(true, Ordering::Relaxed);
                    Some(b"\x1b[3;1R\x1b[?62;22c".to_vec())
                } else {
                    None
                }
            })
            .pty(100, 30);
        p.wait_raw("input box", |b| contains(b, INPUT_BOX));
        p.wait_raw("welcome HOOKED-LINE", |b| contains(b, b"HOOKED-LINE"));
        settle(&p, Duration::from_millis(500), BOOT);
        let mark = p.bytes().len();
        p.prompt("hello", "HOOKED-LINE", |t| t.as_bytes().get(mark..).is_some_and(|s| contains(s, b"HOOKED-LINE")));
        settle(&p, Duration::from_millis(300), BOOT);
        quit_tui(&mut p);
        let after = p.bytes()[mark..].to_vec();
        assert!(contains(&after, b"HOOKED-LINE"), "commit hook output missing");
        assert!(!contains(&after, b"\x1b[?2026h"), "ui/caps set sync = false: no sync brackets");
    };

    let write_paths_are_filtered = || {
        let evil = r"\27]52;c;ZXZpbA==\7\27[2J\27]2;pwned\7";
        let mut p = spawn_tty(
            &[],
            false,
            &[],
            Some(&format!(
                "mare.on(\"ui/ready\", function()\n\
                  mare.ui.set_title(\"TITLE-{evil}-END\")\n\
                  mare.ui.notify_os(\"NOTE-{evil}-END\", {{ protocol = \"osc9\" }})\n\
                  mare.ui.notify(\"FRAME-{evil}-END\")\n\
                end)\n"
            )),
        );
        p.wait_raw("input box", |b| contains(b, INPUT_BOX));
        p.wait_raw("FRAME- and OSC9", |b| contains(b, b"FRAME-") && contains(b, b"\x1b]9;"));
        settle(&p, Duration::from_millis(300), BOOT);
        quit_tui(&mut p);
        let out = p.bytes();
        assert!(contains(&out, b"\x1b]2;") && contains(&out, b"TITLE-"), "title not written");
        assert!(contains(&out, b"\x1b]9;") && contains(&out, b"NOTE-"), "notification not written");
        assert!(contains(&out, b"FRAME-"), "notice not drawn");
        assert!(!contains(&out, b"\x1b]52"), "clipboard write reached the terminal");
        assert!(!contains(&out, b"\x1b]2;pwned"), "nested title reached the terminal");
        assert!(!contains(&out, b"\x1b[2J"), "screen clear reached the terminal");
    };

    let osc52_copy_goes_through_the_write_exit = || {
        let mut p = spawn_tty(
            &[],
            false,
            &[("MARE_TEST_CLIPBOARD", "osc52")],
            Some(
                "mare.on(\"ui/ready\", function()\n\
                  local r = mare.clipboard.copy(\"hello osc52\")\n\
                  local big = mare.clipboard.copy(string.rep(\"x\", mare.clipboard.max_osc52_bytes + 1))\n\
                  mare.ui.notify(\"CB-\" .. tostring(r.method) .. \"-\" .. tostring(r.verified) .. \"-\" .. tostring(big.ok) .. \"-END\")\n\
                end)\n",
            ),
        );
        p.wait_raw("input box", |b| contains(b, INPUT_BOX));
        p.wait_raw("CB-END", |b| contains(b, b"CB-") && contains(b, b"-END"));
        settle(&p, Duration::from_millis(300), BOOT);
        quit_tui(&mut p);
        let out = p.bytes();
        assert!(contains(&out, b"CB-osc52-false-false-END"), "unexpected result");
        assert_eq!(count_subseq(&out, b"\x1b]52;c;aGVsbG8gb3NjNTI=\x07"), 1, "OSC 52 not written once");
        assert_eq!(count_subseq(&out, b"\x1b]52;"), 1, "the over-cap copy reached the terminal");
    };

    std::thread::scope(|s| {
        s.spawn(kitty_query_first);
        s.spawn(idle_writes_nothing);
        s.spawn(resize_redraws_at_the_new_width);
        s.spawn(hostile_output_is_filtered);
        s.spawn(extension_points);
        s.spawn(write_paths_are_filtered);
        s.spawn(osc52_copy_goes_through_the_write_exit);
    });
}

#[cfg(unix)]
#[test]
fn pty_drop_reaps_child_and_removes_temp_dirs() {
    let mut home_path = std::path::PathBuf::new();
    let mut pid = 0u32;
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let cmd = MareCmd::new().no_init().shell_wrap("sh", vec!["-c".to_string(), "sleep 60".to_string()]);
        home_path = cmd.home_path().to_path_buf();
        let p = cmd.pty(80, 24);
        pid = p.pid();
        panic!("simulated test failure");
    }));
    assert!(res.is_err());
    assert!(!home_path.exists(), "temp directory survived panic drop");
    assert_ne!(pid, 0);
    // SAFETY: probing whether the reaped child PID still exists.
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    assert!(!alive, "PTY child {pid} survived Drop");
}

/// `MARE_NO_OSC11` disables ONLY the OSC 11 background
/// query; capability probing (DECRQM, XTVERSION, cell_px, kitty keyboard, DA1) still runs.
#[cfg(unix)]
#[test]
fn mare_no_osc11_only_skips_background_probe_and_still_probes_capabilities() {
    use std::sync::{Arc, Mutex};

    let home = tempfile::tempdir().unwrap();
    let result = home.path().join("caps-result.json");
    let captured_queries = Arc::new(Mutex::new(Vec::<u8>::new()));
    let captured_ref = captured_queries.clone();
    let answered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut p = MareCmd::new()
        .home_dir(home.path())
        .arg("--no-session")
        .env("TERM", "xterm-kitty")
        .env("COLORTERM", "truecolor")
        .env("MARE_NO_OSC11", "1")
        .env_remove("MARE_NO_TERM_PROBE")
        .env_remove("TMUX")
        .env("CAPS_RESULT", &result)
        .init_tail(
            "local bg_count = 0\n\
             local function dump()\n\
               local c = mare.ui.caps()\n\
               c.bg_count = bg_count\n\
               mare.fs.write(mare.env(\"CAPS_RESULT\"), mare.json.encode(c))\n\
             end\n\
             mare.on(\"ui/background\", function() bg_count = bg_count + 1; dump() end)\n\
             mare.on(\"ui/caps-changed\", dump)\n\
             mare.on(\"app/ready\", dump)\n",
        )
        .respond(move |buf| {
            let mut seen = captured_ref.lock().unwrap();
            if buf.len() > seen.len() {
                *seen = buf.to_vec();
            }
            if !answered.load(std::sync::atomic::Ordering::Relaxed) && buf.windows(4).any(|w| w == b"\x1b[6n") {
                answered.store(true, std::sync::atomic::Ordering::Relaxed);
                Some(b"\x1b[?1u\x1b[3;1R\x1b[?62;22c".to_vec())
            } else {
                None
            }
        })
        .pty(100, 30);

    p.wait_raw("kitty_keyboard probed under MARE_NO_OSC11", |_| {
        std::fs::read_to_string(&result)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .is_some_and(|v| v["kitty_keyboard"] == true && v["probed"] == true)
    });
    p.send(b"\x03\x03");
    assert!(p.wait_exit(10), "timed out waiting for PTY child to exit");

    let sent = String::from_utf8_lossy(&captured_queries.lock().unwrap()).into_owned();
    assert!(!sent.contains("\x1b]11;?"), "OSC 11 query must be omitted under MARE_NO_OSC11: {sent:?}");
    assert!(sent.contains("\x1b[?u") && sent.contains("\x1b[c"), "capability probe must still run: {sent:?}");
    let caps: Value = serde_json::from_str(&std::fs::read_to_string(&result).unwrap()).unwrap();
    assert_eq!(caps["kitty_keyboard"], true);
    assert_eq!(caps["probed"], true);
    assert_eq!(caps["bg_count"], 0, "ui/background must not fire under MARE_NO_OSC11");
}

/// when a terminal never responds to OSC 11 / DECRQM / DA1 queries, the startup probe
/// must time out cleanly, publish fallback capabilities, and unblock TUI input without hanging.
#[cfg(unix)]
#[test]
fn unresponsive_terminal_probe_times_out_and_starts_normally() {
    use std::sync::{Arc, Mutex};

    let home = tempfile::tempdir().unwrap();
    let result = home.path().join("caps-timeout.json");
    let captured_queries = Arc::new(Mutex::new(Vec::<u8>::new()));
    let captured_ref = captured_queries.clone();
    let mut p = MareCmd::new()
        .home_dir(home.path())
        .arg("--no-session")
        .env("TERM", "xterm-256color")
        .env_remove("COLORTERM")
        .env_remove("MARE_COLOR")
        .env_remove("NO_COLOR")
        .env_remove("MARE_NO_TERM_PROBE")
        .env_remove("MARE_NO_OSC11")
        .env_remove("TMUX")
        .env("CAPS_RESULT", &result)
        .init_tail(
            "local function dump()\n\
               mare.fs.write(mare.env(\"CAPS_RESULT\"), mare.json.encode(mare.ui.caps()))\n\
             end\n\
             mare.on(\"ui/caps-changed\", dump)\n\
             mare.on(\"app/ready\", dump)\n",
        )
        .respond(move |buf| {
            let mut seen = captured_ref.lock().unwrap();
            if buf.len() > seen.len() {
                *seen = buf.to_vec();
            }
            None
        })
        .pty(100, 30);

    p.wait_raw("fallback caps after unresponsive probe", |_| {
        std::fs::read_to_string(&result)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .is_some_and(|v| v["probed"] == false && v["color_depth"] == "256")
    });
    p.send(b"\x03\x03");
    assert!(p.wait_exit(10), "timed out waiting for PTY child to exit after silent probe");

    let sent = String::from_utf8_lossy(&captured_queries.lock().unwrap()).into_owned();
    assert!(sent.contains("\x1b[c"), "probe query should have been transmitted: {sent:?}");
    let caps: Value = serde_json::from_str(&std::fs::read_to_string(&result).unwrap()).unwrap();
    assert_eq!(caps["probed"], false);
    assert_eq!(caps["kitty_keyboard"], false);
    assert_eq!(caps["color_depth"], "256");
}

/// Regression test:
/// - A `TERM=linux` terminal answering `CSI 6 n` + DA1 (`probed=true`, `probed_truecolor=nil`)
///   keeps `color_depth == "16"` in BOTH `mare.ui.caps()` and `mare.ui.color_depth()`, including
///   after runtime option updates (`mare.o.border = "rounded"`) trigger `set_color_mode(nil)`.
/// - `terminal_caps = { { match = ..., caps = { truecolor = false } } }` drops truecolor to `"256"`
///   instead of being overwritten back to `"truecolor"` in `apply_caps`.
#[test]
fn linux_probed_keeps_16_colors_and_terminal_caps_truecolor_false_applies() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    // Part A: TERM=linux probed (cursor report + DA1) stays "16" across ui.color_depth() & option changes.
    let home_a = tempfile::tempdir().unwrap();
    let res_a = home_a.path().join("caps-a.json");
    let answered_a = Arc::new(AtomicBool::new(false));
    let mut p_a = MareCmd::new()
        .home_dir(home_a.path())
        .arg("--no-session")
        .env("TERM", "linux")
        .env_remove("COLORTERM")
        .env_remove("MARE_COLOR")
        .env_remove("NO_COLOR")
        .env_remove("MARE_NO_OSC11")
        .env_remove("MARE_NO_TERM_PROBE")
        .env_remove("TERM_PROGRAM")
        .env_remove("WT_SESSION")
        .env_remove("KITTY_WINDOW_ID")
        .env_remove("GHOSTTY_RESOURCES_DIR")
        .env_remove("WEZTERM_PANE")
        .env_remove("WEZTERM_EXECUTABLE")
        .env_remove("ITERM_SESSION_ID")
        .env_remove("TMUX")
        .env("CAPS_RESULT", &res_a)
        .init_tail(
            "local stage = 0\n\
             local function record(tag)\n\
               local c = mare.ui.caps()\n\
               c.lua_depth = mare.ui.color_depth()\n\
               c.tag = tag\n\
               mare.fs.write(mare.env(\"CAPS_RESULT\"), mare.json.encode(c))\n\
             end\n\
             mare.on(\"app/ready\", function()\n\
               if stage == 0 then\n\
                 stage = 1\n\
                 record(\"ready\")\n\
               end\n\
             end)\n\
             mare.on(\"ui/caps-changed\", function()\n\
               if stage == 1 then\n\
                 local before_lua = mare.ui.color_depth()\n\
                 mare.o.border = \"rounded\"\n\
                 local c = mare.ui.caps()\n\
                 c.before_lua = before_lua\n\
                 c.after_lua = mare.ui.color_depth()\n\
                 c.tag = \"after_border\"\n\
                 mare.fs.write(mare.env(\"CAPS_RESULT\"), mare.json.encode(c))\n\
               end\n\
             end)\n",
        )
        .respond(move |buf| {
            if !answered_a.load(Ordering::Relaxed) && buf.windows(4).any(|w| w == b"\x1b[6n") {
                answered_a.store(true, Ordering::Relaxed);
                Some(b"\x1b[3;1R\x1b[?6c".to_vec())
            } else {
                None
            }
        })
        .pty(100, 30);

    p_a.wait_raw("probed TERM=linux caps + border option change", |_| {
        std::fs::read_to_string(&res_a)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .is_some_and(|v| v["tag"] == "after_border" && v["probed"] == true)
    });
    p_a.send(b"\x03\x03");
    assert!(p_a.wait_exit(10), "timed out waiting for TERM=linux PTY child");
    let caps_a: Value = serde_json::from_str(&std::fs::read_to_string(&res_a).unwrap()).unwrap();
    assert_eq!(caps_a["before_lua"], "16", "probed TERM=linux ui.color_depth() must stay 16: {caps_a}");
    assert_eq!(caps_a["after_lua"], "16", "after border option change ui.color_depth() must stay 16: {caps_a}");
    assert_eq!(caps_a["color_depth"], "16", "after border option change caps().color_depth must stay 16: {caps_a}");

    // Part B: terminal_caps = { { match = ..., caps = { truecolor = false } } } disables truecolor.
    let home_b = tempfile::tempdir().unwrap();
    let res_b = home_b.path().join("caps-b.json");
    let mut p_b = MareCmd::new()
        .home_dir(home_b.path())
        .arg("--no-session")
        .env("TERM", "xterm-256color")
        .env("MARE_NO_TERM_PROBE", "1")
        .env("TERM_PROGRAM", "WezTerm")
        .env_remove("COLORTERM")
        .env_remove("MARE_COLOR")
        .env_remove("NO_COLOR")
        .env("CAPS_RESULT", &res_b)
        .init_tail(
            "mare.setup { terminal_caps = { { match = { TERM_PROGRAM = \"WezTerm\" }, caps = { truecolor = false } } } }\n\
             mare.on(\"ui/caps-changed\", function()\n\
               local c = mare.ui.caps()\n\
               c.lua_depth = mare.ui.color_depth()\n\
               mare.fs.write(mare.env(\"CAPS_RESULT\"), mare.json.encode(c))\n\
             end)\n",
        )
        .pty(100, 30);

    p_b.wait_raw("terminal_caps truecolor=false override", |_| {
        std::fs::read_to_string(&res_b)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .is_some_and(|v| v.get("color_depth").is_some())
    });
    p_b.send(b"\x03\x03");
    assert!(p_b.wait_exit(10), "timed out waiting for terminal_caps PTY child");
    let caps_b: Value = serde_json::from_str(&std::fs::read_to_string(&res_b).unwrap()).unwrap();
    assert_eq!(caps_b["truecolor"], false, "terminal_caps truecolor=false must take effect: {caps_b}");
    assert_eq!(caps_b["color_depth"], "256", "terminal_caps truecolor=false must drop to 256: {caps_b}");
    assert_eq!(caps_b["lua_depth"], "256", "ui.color_depth() must agree after terminal_caps truecolor=false: {caps_b}");
}

/// Regression test:
/// WT_SESSION (even with empty TERM) and TERM_PROGRAM + TERM=xterm-256color without COLORTERM keep
/// default TrueColor ("truecolor") when probe is skipped/unanswered.
#[test]
fn wt_session_and_term_program_with_256_term_keep_truecolor_by_default() {
    for (label, extra_env) in [
        ("WT_SESSION", vec![("WT_SESSION", "wt-guid"), ("TERM", "")]),
        ("TERM_PROGRAM+xterm-256color", vec![("TERM_PROGRAM", "WezTerm"), ("TERM", "xterm-256color")]),
    ] {
        let home = tempfile::tempdir().unwrap();
        let res = home.path().join("caps.json");
        let mut cmd = MareCmd::new()
            .home_dir(home.path())
            .arg("--no-session")
            .env("MARE_NO_TERM_PROBE", "1")
            .env_remove("COLORTERM")
            .env_remove("MARE_COLOR")
            .env_remove("NO_COLOR")
            .env_remove("TERM_PROGRAM")
            .env_remove("WT_SESSION")
            .env_remove("KITTY_WINDOW_ID")
            .env_remove("GHOSTTY_RESOURCES_DIR")
            .env_remove("WEZTERM_PANE")
            .env_remove("WEZTERM_EXECUTABLE")
            .env_remove("ITERM_SESSION_ID")
            .env("CAPS_RESULT", &res);
        for (k, v) in extra_env {
            cmd = cmd.env(k, v);
        }
        let mut p = cmd
            .init_tail(
                "mare.on(\"ui/caps-changed\", function()\n\
                   local c = mare.ui.caps()\n\
                   c.lua_depth = mare.ui.color_depth()\n\
                   mare.fs.write(mare.env(\"CAPS_RESULT\"), mare.json.encode(c))\n\
                 end)\n",
            )
            .pty(100, 30);

        p.wait_raw(&format!("truecolor caps under {label}"), |_| {
            std::fs::read_to_string(&res)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .is_some_and(|v| v.get("color_depth").is_some())
        });
        p.send(b"\x03\x03");
        assert!(p.wait_exit(10), "timed out waiting for {label} PTY child");
        let caps: Value = serde_json::from_str(&std::fs::read_to_string(&res).unwrap()).unwrap();
        assert_eq!(caps["color_depth"], "truecolor", "{label}: {caps}");
        assert_eq!(caps["lua_depth"], "truecolor", "{label}: {caps}");
    }
}
