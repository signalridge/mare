//! Terminal capabilities: the startup probe, the caps table, `ui/caps`.

use crate::app::*;

/// The shared terminal response budget.
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

// The write queue has its own bound; it is not part of the terminal response budget.
#[cfg(unix)]
const PROBE_FLUSH_TIMEOUT: Duration = Duration::from_millis(100);

impl App {
    /// Terminal capabilities: static table + probe answers + the user's `terminal_caps` entries,
    /// stored for `mare.ui.caps()` and passed through the `ui/caps` waterfall.
    pub(in crate::app) fn publish_caps(&mut self, probe: &mare_tui::term_input::Probe) {
        let env = |k: &str| std::env::var(k).ok();
        let mux = if env("ZELLIJ").is_some() {
            "zellij"
        } else if env("TMUX").is_some() {
            "tmux"
        } else if env("STY").is_some() {
            "screen"
        } else {
            ""
        };
        let cfg = self.host.kernel.config();
        let term_color_opt = cfg.pointer("/terminal/color").and_then(|v| v.as_str()).map(str::to_string);
        let icons_opt = cfg.get("icons").and_then(|v| v.as_str()).map(str::to_string);
        let color_mode = mare_shared::style::ColorMode::resolve(term_color_opt.as_deref(), &env, probe.truecolor);
        let detected_emoji_wide = mare_tui::termcaps::detect(&env).is_none_or(|c| c.emoji_wide);
        let resolved_icons = mare_shared::style::resolve_icons(icons_opt.as_deref(), &env, Some(detected_emoji_wide));
        let mut caps = serde_json::json!({
            "sync": self.renderer.caps.sync,
            "scroll_region": self.renderer.caps.scroll_region,
            "position_known": self.renderer.scroll_insert_ready(),
            "color_depth": color_mode.as_str(),
            "truecolor": color_mode == ColorMode::TrueColor,
            "no_color": color_mode == ColorMode::NoColor,
            "icons": resolved_icons,
            "grapheme": probe.grapheme.unwrap_or(false),
            "kitty_keyboard": probe.kitty.is_some(),
            "probed": probe.cursor_row.is_some() || probe.sync.is_some(),
            "probed_truecolor": probe.truecolor,
            "term": env("TERM").unwrap_or_default(),
            "term_program": env("TERM_PROGRAM").unwrap_or_default(),
            "multiplexer": mux,
            // the static capability table: Terminal.app before macOS Tahoe
            // draws many emoji one cell wide (the ui plugin picks ASCII icons there)
            "emoji_wide": detected_emoji_wide,
            "terminal": mare_tui::termcaps::detect(&env).map(|c| c.name).unwrap_or(""),
            // the width rule: MARE_WIDTH_METHOD, then the mode 2027 answer, then
            // the table — "unicode" (grapheme clusters) or "wcwidth" (code point by code point)
            "width_method": mare_tui::termcaps::width_method(&env, probe.grapheme_set).name(),
            // capabilities: image protocol, OSC 8 links, cell size, what the probe heard
            "images": image_protocol(&cfg, &env, Some(probe)).name(),
            "hyperlinks": hyperlinks_by_env(&env),
            "xtversion": probe.xtversion.clone().unwrap_or_default(),
            "da1": probe.da1.clone(),
            "cell_px": probe.cell_px.map(|(w, h)| serde_json::json!([w, h])).unwrap_or(serde_json::Value::Null),
        });
        // `terminal_caps = { { match = { TERM_PROGRAM = "…" }, caps = { sync = false } }, … }`
        if let Some(entries) = self.host.kernel.config().get("terminal_caps").and_then(|v| v.as_array().cloned()) {
            for e in entries {
                let matches = e.get("match").and_then(|m| m.as_object()).is_none_or(|m| {
                    m.iter().all(|(k, v)| env(k).is_some_and(|have| v.as_str().is_some_and(|want| have.contains(want))))
                });
                if let (true, Some(over)) = (matches, e.get("caps").and_then(|c| c.as_object())) {
                    for (k, v) in over {
                        caps[k] = v.clone();
                    }
                }
            }
        }
        self.apply_caps(caps.clone());
        if self.host.kernel.listener_count("ui/caps") > 0 {
            let (h, tx) = (self.host.clone(), self.app_tx.clone());
            tokio::task::spawn_local(async move {
                let lua = h.lua();
                let Ok(v) = mare_shared::lua::json_to_lua(lua, &caps) else { return };
                let out = h.kernel.waterfall("ui/caps", v, mlua::MultiValue::new()).await;
                if let Ok(j) = mare_shared::lua::lua_to_json(lua, out)
                    && j.is_object()
                {
                    let _ = tx.send(AppMsg::Caps(j));
                }
            });
        }
    }

    pub(in crate::app) fn apply_caps(&mut self, mut caps: serde_json::Value) {
        let env = |k: &str| std::env::var(k).ok();
        let cfg = self.host.kernel.config();
        let term_color_opt = cfg.pointer("/terminal/color").and_then(|v| v.as_str()).map(str::to_string);
        let icons_opt = cfg.get("icons").and_then(|v| v.as_str()).map(str::to_string);

        if let Some(b) = caps.get("sync").and_then(|v| v.as_bool()) {
            self.renderer.caps.sync = b;
        }
        if let Some(b) = caps.get("scroll_region").and_then(|v| v.as_bool()) {
            self.renderer.caps.scroll_region = b;
        }
        let explicit_opt = term_color_opt.as_deref().and_then(ColorMode::parse);
        let cap_probed_tc = caps.get("probed_truecolor").and_then(|v| v.as_bool());
        let base_color = ColorMode::resolve(term_color_opt.as_deref(), &env, cap_probed_tc);
        let resolved_color = if let Some(m) = explicit_opt {
            m
        } else if mare_shared::style::no_color_env(&env) {
            ColorMode::NoColor
        } else {
            let cd = caps.get("color_depth").and_then(|v| v.as_str()).and_then(ColorMode::parse);
            let tc = caps.get("truecolor").and_then(|v| v.as_bool());
            if cd.is_some_and(|m| m != base_color) {
                cd.unwrap()
            } else if tc == Some(false) && base_color == ColorMode::TrueColor {
                ColorMode::Ansi256
            } else if tc == Some(true) && base_color != ColorMode::TrueColor {
                ColorMode::TrueColor
            } else {
                cd.unwrap_or(base_color)
            }
        };
        self.renderer.color = resolved_color;
        caps["color_depth"] = serde_json::json!(resolved_color.as_str());
        caps["truecolor"] = serde_json::json!(resolved_color == ColorMode::TrueColor);
        caps["no_color"] = serde_json::json!(resolved_color == ColorMode::NoColor);
        if resolved_color != base_color && explicit_opt.is_none() && !mare_shared::style::no_color_env(&env) {
            caps["color_depth_override"] = serde_json::json!(resolved_color.as_str());
        } else if let Some(obj) = caps.as_object_mut() {
            obj.remove("color_depth_override");
        }

        let emoji_wide = caps.get("emoji_wide").and_then(|v| v.as_bool());
        caps["icons"] = serde_json::json!(mare_shared::style::resolve_icons(icons_opt.as_deref(), &env, emoji_wide));
        if let Some(m) =
            caps.get("width_method").and_then(|v| v.as_str()).and_then(mare_shared::style::WidthMethod::parse)
        {
            mare_shared::style::set_width_method(m);
        }
        if let Some(p) = caps.get("images").and_then(|v| v.as_str()).and_then(mare_tui::image::Protocol::parse) {
            mare_tui::image::set_protocol(p);
        }
        if let Some([w, h]) = caps
            .get("cell_px")
            .and_then(|v| v.as_array())
            .map(|a| [a.first().and_then(|v| v.as_u64()).unwrap_or(0), a.get(1).and_then(|v| v.as_u64()).unwrap_or(0)])
        {
            if w > 0 && h > 0 {
                self.probe_state.cell_px = Some((w as u32, h as u32));
            }
            mare_tui::image::set_cell_px(w as u32, h as u32);
        }
        if let Some(b) = caps.get("hyperlinks").and_then(|v| v.as_bool()) {
            self.ui.theme.write().unwrap().md.osc8 = b;
            mare_shared::style::theme_changed();
        }
        *self.ui.caps.write().unwrap() = caps.clone();
        self.emit("ui/caps-changed", caps);
    }
}

/// Read the startup probe's replies (the query itself went out through the write exit, in
/// order with the frames). Waits for the DA1 fence with a hard deadline (`select(2)`, which works
/// on macOS ttys). Runs once, before the event reader starts.
#[cfg(unix)]
pub(in crate::app) fn read_probe() -> ProbeReply {
    // Give the queued query a bounded flush window before starting the response deadline.
    mare_tui::out::wait_idle(PROBE_FLUSH_TIMEOUT);
    read_probe_from_fd(0)
}

#[cfg(unix)]
fn read_probe_from_fd(fd: libc::c_int) -> ProbeReply {
    read_probe_with(
        |left| {
            let ready = unsafe {
                let mut set: libc::fd_set = std::mem::zeroed();
                libc::FD_ZERO(&mut set);
                libc::FD_SET(fd, &mut set);
                let mut tv = libc::timeval { tv_sec: 0, tv_usec: left.as_micros().min(999_999) as _ };
                libc::select(fd + 1, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &mut tv)
            };
            if ready < 0 { Err(std::io::Error::last_os_error()) } else { Ok(ready > 0) }
        },
        |chunk| {
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
        },
    )
}

#[cfg(unix)]
fn read_probe_with(
    mut select: impl FnMut(Duration) -> std::io::Result<bool>,
    mut read: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
) -> ProbeReply {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut buf: Vec<u8> = Vec::with_capacity(128);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match select(left) {
            Ok(true) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            _ => break,
        }
        let mut chunk = [0u8; 512];
        let n = match read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        // the DA1 reply (`ESC [ ? … c`) comes last: stop as soon as it arrived
        if mare_tui::term_input::probe_complete(&buf) {
            break;
        }
    }
    let rgb = parse_osc11(&String::from_utf8_lossy(&buf));
    (rgb, mare_tui::term_input::parse_probe(&buf), strip_replies(&buf))
}

#[cfg(not(unix))]
pub(in crate::app) fn read_probe() -> ProbeReply {
    (None, Default::default(), vec![])
}

/// Probe result: background RGB (OSC 11), the capability answers, keys typed meanwhile.
pub(in crate::app) type ProbeReply = (Option<[u8; 3]>, mare_tui::term_input::Probe, Vec<u8>);

/// OSC 8 hyperlinks by environment: only terminals known
/// to render them; unknown ones get `text (url)` so the URL never disappears.
pub(in crate::app) fn hyperlinks_by_env(var: &dyn Fn(&str) -> Option<String>) -> bool {
    let lower = |k: &str| var(k).unwrap_or_default().to_ascii_lowercase();
    let (prog, term) = (lower("TERM_PROGRAM"), lower("TERM"));
    if var("TMUX").is_some() || term.starts_with("tmux") || term.starts_with("screen") {
        return false;
    }
    if lower("TERMINAL_EMULATOR") == "jetbrains-jediterm" {
        return false;
    }
    ["kitty", "ghostty", "wezterm", "warpterminal", "iterm.app", "alacritty", "vscode", "zed"].contains(&prog.as_str())
        || var("KITTY_WINDOW_ID").is_some()
        || var("WEZTERM_PANE").is_some()
        || var("GHOSTTY_RESOURCES_DIR").is_some()
        || var("ITERM_SESSION_ID").is_some()
        || var("WT_SESSION").is_some()
        || var("ALACRITTY_WINDOW_ID").is_some()
        || term.contains("kitty")
        || term.contains("ghostty")
        || term.starts_with("alacritty")
}

/// Image protocol: `images_protocol` wins, else environment then probe.
pub(in crate::app) fn image_protocol(
    config: &serde_json::Value,
    var: &dyn Fn(&str) -> Option<String>,
    probe: Option<&mare_tui::term_input::Probe>,
) -> mare_tui::image::Protocol {
    use mare_tui::image::{Protocol, detect};
    if let Some(p) = config.get("images_protocol").and_then(|v| v.as_str()).and_then(Protocol::parse) {
        return p;
    }
    match probe {
        Some(p) => detect(var, p.xtversion.as_deref(), &p.da1),
        None => mare_tui::image::detect_env(var).unwrap_or(Protocol::None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_timeout_is_bounded() {
        assert_eq!(PROBE_TIMEOUT, Duration::from_millis(250));
    }

    #[cfg(unix)]
    #[test]
    fn probe_reads_replies_and_replays_typed_bytes_at_the_fence() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"h\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\i\x1b[12;1R\x1b[?62;22c").unwrap();
        let (rgb, probe, typed) = read_probe_from_fd(reader.as_raw_fd());
        assert_eq!(rgb, Some([28, 28, 28]));
        assert_eq!(probe.cursor_row, Some(12));
        assert_eq!(probe.da1, vec![62, 22]);
        assert_eq!(typed, b"hi");
    }

    #[cfg(unix)]
    #[test]
    fn probe_retries_interrupted_select_and_read_preserving_partial_reply() {
        let mut selects = 0;
        let mut reads = 0;
        let (rgb, probe, typed) = read_probe_with(
            |_| {
                selects += 1;
                if selects == 2 { Err(std::io::ErrorKind::Interrupted.into()) } else { Ok(true) }
            },
            |buf| {
                reads += 1;
                let part: &[u8] = match reads {
                    1 => b"h\x1b]11;rgb:1c1c/",
                    2 => return Err(std::io::ErrorKind::Interrupted.into()),
                    _ => b"1c1c/1c1c\x1b\\i\x1b[12;1R\x1b[?62;22c",
                };
                buf[..part.len()].copy_from_slice(part);
                Ok(part.len())
            },
        );
        assert_eq!(rgb, Some([28, 28, 28]));
        assert_eq!(probe.da1, vec![62, 22]);
        assert_eq!(typed, b"hi");
    }

    #[cfg(unix)]
    #[test]
    fn probe_stops_cleanly_on_select_timeout_or_read_eof_and_image_protocol_respects_config() {
        let (rgb, probe, typed) = read_probe_with(|_| Ok(false), |_| unreachable!("read should not run"));
        assert_eq!((rgb, probe.da1.len(), typed.len()), (None, 0, 0));

        let (rgb2, probe2, typed2) = read_probe_with(|_| Ok(true), |_| Ok(0));
        assert_eq!((rgb2, probe2.da1.len(), typed2.len()), (None, 0, 0));

        let (rgb3, probe3, typed3) =
            read_probe_with(|_| Ok(true), |_| Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)));
        assert_eq!((rgb3, probe3.da1.len(), typed3.len()), (None, 0, 0));

        let proto = image_protocol(&serde_json::json!({ "images_protocol": "kitty" }), &|_| None, None);
        assert_eq!(proto, mare_tui::image::Protocol::Kitty);
    }
}
