//! Terminal initialization and post-first-frame capability probing.

use super::caps::{PROBE_TIMEOUT, ProbeReply, hyperlinks_by_env, image_protocol, read_probe};
use super::term_guard::TermGuard;
use crate::app::*;

pub(in crate::app) struct TermInit {
    pub(in crate::app) width: u16,
    pub(in crate::app) height: u16,
    pub(in crate::app) margin: usize,
    pub(in crate::app) guard: TermGuard,
}

/// Open the tty, seed environment-based capabilities, enter raw/live mode, and start stdio capture.
pub(in crate::app) fn init_terminal(ui: &UiShared, config: &serde_json::Value) -> Result<TermInit> {
    // links and images by environment until the startup probe answers
    {
        let env = |k: &str| std::env::var(k).ok();
        ui.theme.write().unwrap().md.osc8 = hyperlinks_by_env(&env);
        mare_tui::image::set_protocol(image_protocol(config, &env, None));
    }
    // the terminal is opened here and kept until the TUI ends (raw mode, size, input)
    tty::open()?;
    let (width, height) = match tty::window_size() {
        Ok(ws) => {
            if let (Some(pw), Some(ph)) = (ws.pixel_width, ws.pixel_height)
                && ws.cols > 0
                && ws.rows > 0
            {
                let (cw, ch) = (pw as u32 / ws.cols as u32, ph as u32 / ws.rows as u32);
                if cw > 0 && ch > 0 {
                    mare_tui::image::set_cell_px(cw, ch);
                }
            }
            (ws.cols, ws.rows)
        }
        Err(_) => (100, 40),
    };
    // plugins lay out at the width the core draws at (the tmux / screen margin taken off)
    let margin = mare_tui::termcaps::line_end_margin(&|k| std::env::var(k).ok());
    ui.width.store(width.saturating_sub(margin as u16).max(1), std::sync::atomic::Ordering::Relaxed);
    ui.height.store(height, std::sync::atomic::Ordering::Relaxed);
    // the width rule from the environment until the probe's mode 2027 answer refines it
    mare_shared::style::set_width_method(mare_tui::termcaps::width_method(&|k| std::env::var(k).ok(), None));

    let guard = TermGuard::enter()?;
    // stdout/stderr writes from inside mare (plugin `print`, libraries, inherited child stdio)
    // are captured from here on and committed above the live area
    mare_tui::capture::start();
    Ok(TermInit { width, height, margin, guard })
}

fn probe_settings(host: &Host) -> (bool, bool) {
    let cfg = host.kernel.config();
    let probe_enabled = std::env::var_os("MARE_NO_TERM_PROBE").is_none()
        && cfg.pointer("/terminal/probe/enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let osc11_cfg = cfg.pointer("/terminal/probe/background").and_then(|v| v.as_bool()).unwrap_or(true);
    let flavor = cfg.get("flavor").and_then(|v| v.as_str()).map(str::to_string);
    let osc11 =
        osc11_cfg && std::env::var_os("MARE_NO_OSC11").is_none() && flavor.as_deref().unwrap_or("auto") == "auto";
    (probe_enabled, osc11)
}

/// Start the post-first-frame terminal capability probe (OSC 11, DECRQM, kitty keyboard, DA1).
///
/// If `events` is already open or probing is disabled via env, publishes default capabilities
/// immediately and ensures `events` is ready.
pub(in crate::app) fn start_probe(
    app: &mut App,
    events: &mut Option<tty::Events>,
) -> Option<tokio::task::JoinHandle<ProbeReply>> {
    let (probe_enabled, osc11) = probe_settings(&app.host);
    // (Unix: written through the write exit and read from stdin; Windows: the same
    // query written and read through the console handles — see below)
    if events.is_some() {
        app.publish_caps(&app.probe_state.clone());
        return None;
    }
    if probe_enabled {
        if cfg!(unix) {
            let wl = mare_tui::term_input::query_whitelisted(&|k| std::env::var(k).ok());
            mare_tui::out::write_own(mare_tui::term_input::probe_query(osc11, wl));
            return Some(tokio::task::spawn_blocking(read_probe));
        }
        if cfg!(windows) {
            // Windows: the same probe (background, DECRQM, kitty keyboard flags, cursor
            // position, DA1) through the raw console handles, before the key reader starts so
            // the replies aren't read as keys; a console that doesn't answer OSC 11 has its color table
            let wl = mare_tui::term_input::query_whitelisted(&|k| std::env::var(k).ok());
            let query = mare_tui::term_input::probe_query(osc11, wl);
            return Some(tokio::task::spawn_blocking(move || {
                let buf = mare_tui::winprobe::probe(&query, PROBE_TIMEOUT).unwrap_or_default();
                let rgb = mare_tui::term_input::parse_osc11(&String::from_utf8_lossy(&buf))
                    .or_else(|| if osc11 { mare_tui::winprobe::console_background() } else { None });
                let typed = mare_tui::term_input::strip_replies(&buf);
                (rgb, mare_tui::term_input::parse_probe(&buf), typed)
            }));
        }
    }
    app.publish_caps(&app.probe_state.clone());
    match tty::events() {
        Ok(ev) => *events = Some(ev),
        Err(_) => {
            *events = None;
            app.quit = true;
        }
    }
    None
}

/// Merge an incremental or full probe result into `app.probe_state` and apply all side effects
/// (cursor anchor, sync output, truecolor, kitty keyboard push, cell_px, `ui/caps`, `ui/background`).
pub(in crate::app) fn apply_probe_update(
    app: &mut App,
    host: &Arc<Host>,
    guard: Option<&TermGuard>,
    rgb: Option<[u8; 3]>,
    delta: mare_tui::term_input::Probe,
    saw_da1: bool,
) {
    if delta.sync.is_some() {
        app.probe_state.sync = delta.sync;
    }
    if delta.grapheme.is_some() {
        app.probe_state.grapheme = delta.grapheme;
    }
    if delta.grapheme_set.is_some() {
        app.probe_state.grapheme_set = delta.grapheme_set;
    }
    if delta.truecolor.is_some() {
        app.probe_state.truecolor = delta.truecolor;
    }
    if delta.kitty.is_some() {
        app.probe_state.kitty = delta.kitty;
    }
    if delta.cursor_row.is_some() {
        app.probe_state.cursor_row = delta.cursor_row;
    }
    if delta.xtversion.is_some() {
        app.probe_state.xtversion = delta.xtversion;
    }
    if !delta.da1.is_empty() {
        app.probe_state.da1 = delta.da1;
    }
    if delta.cell_px.is_some() {
        app.probe_state.cell_px = delta.cell_px;
    }
    if saw_da1 || !app.probe_state.da1.is_empty() {
        app.probe_complete = true;
    }

    let probe = app.probe_state.clone();
    // the live area's absolute position (scroll-region insertion), sync output,
    // truecolor, kitty keyboard (enabled only now that the terminal answered)
    if let Some(row) = delta.cursor_row {
        app.renderer.anchor(row);
    }
    if probe.sync == Some(false) {
        app.renderer.caps.sync = false;
    }
    let term_color = host.kernel.config().pointer("/terminal/color").and_then(|v| v.as_str()).map(str::to_string);
    app.renderer.color =
        mare_shared::style::ColorMode::resolve(term_color.as_deref(), &|k| std::env::var(k).ok(), probe.truecolor);
    if probe.kitty.is_some()
        && let Some(guard) = guard
    {
        guard.enable_kitty();
    }
    let img_gen_before = mare_tui::image::generation();
    app.publish_caps(&probe);
    if mare_tui::image::generation() != img_gen_before {
        app.transcript.invalidate_cache();
        app.reflow.invalidated(tokio::time::Instant::now().into_std());
    }
    let (_, osc11) = probe_settings(host);
    if osc11 && let Some(rgb @ [r, g, b]) = rgb {
        let h = host.clone();
        // {dark, r, g, b}: themes pick a flavor and blend local backgrounds
        let ev = serde_json::json!({ "dark": is_dark(rgb), "r": r, "g": g, "b": b });
        tokio::task::spawn_local(async move {
            h.emit_json("ui/background", ev).await;
        });
    }
}

/// Apply the completed terminal probe reply, emit `ui/background`, replay buffered events
/// (preserving bracketed paste and focus transitions), and open the tty event stream.
pub(in crate::app) fn apply_probe_reply(
    app: &mut App,
    host: &Arc<Host>,
    guard: &TermGuard,
    reply: ProbeReply,
) -> Option<tty::Events> {
    let (rgb, probe, typed) = reply;
    let saw_da1 = !probe.da1.is_empty();
    apply_probe_update(app, host, Some(guard), rgb, probe, saw_da1);
    if !typed.is_empty() {
        for ev in decode_events(&typed) {
            match ev {
                Event::Key(k) => app.handle_key(k),
                other => super::super::event_loop::handle_event(app, &app.ui.clone(), other),
            }
        }
    }
    match tty::events() {
        Ok(ev) => Some(ev),
        Err(_) => {
            app.quit = true;
            None
        }
    }
}

/// Route a late or re-probed terminal escape reply arriving in-band on `tty::Events`.
pub(in crate::app) fn apply_inband_escape(app: &mut App, guard: Option<&TermGuard>, ev: &Event) -> bool {
    let Some(parsed) = mare_tui::term_input::parse_escape_event(ev) else {
        return false;
    };
    let host = app.host.clone();
    apply_probe_update(app, &host, guard, parsed.rgb, parsed.probe, parsed.saw_da1);
    if parsed.theme_report {
        // DEC 2031 reports only dark vs light; follow up with an in-band OSC 11 query through
        // the shared background-refresh path so supporting terminals supply their exact RGB bg.
        refresh_background_inband(app);
    }
    true
}

/// Route a split in-band terminal reply reassembled by `EscJoin` (`OSC 11`, `DCS`, `APC`, `CSI`
/// probe replies, and `CSI row;col R` cursor-position reports).
pub(in crate::app) fn apply_inband_reply_bytes(app: &mut App, guard: Option<&TermGuard>, bytes: &[u8]) {
    let rgb = mare_tui::term_input::parse_osc11(&String::from_utf8_lossy(bytes));
    let mut probe = mare_tui::term_input::parse_probe(bytes);
    let saw_da1 = !probe.da1.is_empty();
    if let Some(row) = probe.cursor_row.take() {
        app.handle_cpr_reply(row);
    }
    if rgb.is_some() || saw_da1 || probe != mare_tui::term_input::Probe::default() {
        let host = app.host.clone();
        apply_probe_update(app, &host, guard, rgb, probe, saw_da1);
    }
}

/// If the startup probe timed out before receiving DA1, send one non-blocking in-band re-probe
/// when the terminal regains focus or resizes (replies route through [`apply_inband_escape`]).
pub(in crate::app) fn maybe_reprobe_inband(app: &mut App) {
    if !cfg!(unix) || mare_tui::modes::handed_off() || app.probe_complete || app.late_reprobed {
        return;
    }
    let (probe_enabled, osc11) = probe_settings(&app.host);
    if !probe_enabled {
        return;
    }
    app.late_reprobed = true;
    let wl = mare_tui::term_input::query_whitelisted(&|k| std::env::var(k).ok());
    mare_tui::out::write_own(mare_tui::term_input::probe_query(osc11, wl));
}

/// Send one nonblocking in-band OSC 11 background query after regaining the foreground
/// (e.g. `Ctrl+Z` / `fg`), honoring the existing probe and `flavor = "auto"` settings.
/// Replies route through [`apply_inband_escape`] / [`apply_inband_reply_bytes`] -> `ui/background`.
pub(in crate::app) fn refresh_background_inband(app: &App) {
    if !cfg!(unix) || mare_tui::modes::handed_off() {
        return;
    }
    let (probe_enabled, osc11) = probe_settings(&app.host);
    if !probe_enabled || !osc11 {
        return;
    }
    mare_tui::out::write_own(mare_tui::term_input::OSC11_QUERY.to_vec());
}
