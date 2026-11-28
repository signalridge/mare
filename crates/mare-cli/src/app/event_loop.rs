//! The main event loop (`run`), non-key events, the resume hint on exit.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use super::startup::boot::{BootOutcome, boot_host};
use super::startup::install_input::StartupInput;
use super::term::caps::ProbeReply;
use super::term::probe::{apply_probe_reply, init_terminal, start_probe};
use super::term::term_guard::{TermGuard, suspend};
use super::*;

#[cfg(unix)]
type AskpassGuard = Option<crate::askpass::Guard>;
#[cfg(not(unix))]
type AskpassGuard = ();

/// Cache sizes from the merged config:
/// `perf = { highlight_cache, markdown_cache, wrap_cache, preview_cache }`.
fn apply_perf(config: &serde_json::Value) {
    let perf = config.get("perf");
    if let Some(n) = perf.and_then(|p| p.get("highlight_cache")).and_then(|v| v.as_u64()) {
        mare_tui::highlight::set_cache_capacity(n as usize);
    }
    for c in [&mare_tui::render::cache::MARKDOWN, &mare_tui::render::cache::WRAP, &mare_tui::render::cache::PREVIEW] {
        if let Some(n) = perf.and_then(|p| p.get(format!("{}_cache", c.name()))).and_then(|v| v.as_u64()) {
            c.set_capacity(n as usize);
        }
    }
}

fn apply_host_config(app: &mut App, config: &serde_json::Value) {
    // width rules appended in init.lua (`char_widths`)
    if let Some(list) = config.get("char_widths").and_then(|v| v.as_array().cloned()) {
        let ranges = list
            .iter()
            .filter_map(|e| {
                let w = e.get("width")?.as_u64()? as u8;
                if let Some(c) = e.get("char").and_then(|c| c.as_str()).and_then(|c| c.chars().next()) {
                    return Some((c as u32, c as u32, w));
                }
                Some((e.get("from")?.as_u64()? as u32, e.get("to")?.as_u64()? as u32, w))
            })
            .collect();
        mare_shared::style::set_width_overrides(ranges);
    }
    if let Some(list) = config.get("key_conflicts").and_then(|v| v.as_array().cloned()) {
        for e in list {
            let s = |k: &str| e.get(k).and_then(|v| v.as_str()).map(str::to_string);
            if let Some(key) = s("key") {
                app.conflict_table.push(mare_tui::keyconflict::Entry {
                    key,
                    terminal: s("terminal").unwrap_or_else(|| "any".into()),
                    by: s("by").unwrap_or_else(|| "the terminal or OS".into()),
                    fix: s("fix").unwrap_or_else(|| "rebind it in mare".into()),
                });
            }
        }
    }
    if let Some(b) = config.get("paste_burst").and_then(|v| v.as_bool()) {
        app.burst_enabled = b;
    }
    apply_perf(config);
}

fn init_app(
    host: &Arc<Host>,
    ui: &Arc<UiShared>,
    app_tx: mpsc::UnboundedSender<AppMsg>,
    boot_input: Option<Box<StartupInput>>,
    term: &super::term::probe::TermInit,
    initial_prompt: Option<String>,
) -> App {
    let (term_w, term_h, margin) = (term.width, term.height, term.margin);
    // Re-query live terminal dimensions after boot / startup_error / external editor handoff.
    let mut initial_cell_px = None;
    let (w, h) = match tty::window_size() {
        Ok(ws) => {
            if let (Some(pw), Some(ph)) = (ws.pixel_width, ws.pixel_height)
                && ws.cols > 0
                && ws.rows > 0
            {
                let (cw, ch) = (pw as u32 / ws.cols as u32, ph as u32 / ws.rows as u32);
                if cw > 0 && ch > 0 {
                    initial_cell_px = Some((cw, ch));
                    mare_tui::image::set_cell_px(cw, ch);
                }
            }
            (ws.cols, ws.rows)
        }
        Err(_) => boot_input.as_ref().map(|input| (input.width, input.height)).unwrap_or((term_w, term_h)),
    };
    ui.width.store(w.saturating_sub(margin as u16).max(1), std::sync::atomic::Ordering::Relaxed);
    ui.height.store(h, std::sync::atomic::Ordering::Relaxed);
    let mut app = App::new(host.clone(), ui.clone(), app_tx, w, h);
    if let Some(px) = initial_cell_px {
        app.probe_state.cell_px = Some(px);
    }
    if let Some(input) = boot_input {
        // Keep what was typed on the install screen, but preserve App::new's configured
        // Renderer (terminal.color and full physical terminal width w).
        app.editor.replace_editor(input.editor);
        app.sync_editor_state();
    }
    app.renderer.height = h as usize;
    app.margin = margin;
    app.width = app.layout_width(w) as usize;
    mare_shared::profile::mark("app state");
    // one short line per startup failure; the tracebacks are in /plugins
    for (p, level) in host.startup_notices() {
        app.push_notice((p, level.into(), Instant::now()));
    }
    // ambiguous bindings are a configuration error, reported at load
    for m in app.ambiguous_keys() {
        host.kernel.add_problem(m.clone());
        app.push_notice((m, "error".into(), Instant::now()));
    }
    // the command-line prompt goes once trust and session startup finish after the first frame
    if let Some(prompt) = initial_prompt {
        app.startup_prompt(prompt);
    }
    apply_host_config(&mut app, &host.kernel.config());
    app
}

/// Non-key terminal events (paste, focus, resize).
pub(super) fn handle_event(app: &mut App, ui: &UiShared, ev: Event) {
    use std::sync::atomic::Ordering::Relaxed;
    match ev {
        Event::Paste(s) => app.handle_paste(&s),
        // focus is unknown until the terminal reports it (`focus_known`, mare.ui.notify_os)
        Event::FocusIn => {
            ui.focused.store(true, Relaxed);
            ui.focus_known.store(true, Relaxed);
            super::term::probe::maybe_reprobe_inband(app);
        }
        Event::FocusOut => {
            ui.focused.store(false, Relaxed);
            ui.focus_known.store(true, Relaxed);
        }
        Event::Mouse(m) => app.handle_mouse(m),
        Event::WindowResized(size) => {
            let (nw, nh) = (size.cols, size.rows);
            let now = mare_tui::render::clock_now();
            app.apply_terminal_size(nw, nh, now);
            if let (Some(pw), Some(ph)) = (size.pixel_width, size.pixel_height)
                && nw > 0
                && nh > 0
            {
                let (cw, ch) = (pw as u32 / nw as u32, ph as u32 / nh as u32);
                app.update_cell_px(cw, ch, now);
            }
            super::term::probe::maybe_reprobe_inband(app);
        }
        other => {
            if let Some(row) = other.cursor_position_row() {
                app.handle_cpr_reply(row);
            }
        }
    }
}

const ANIM_LATE_WARN: Duration = Duration::from_millis(50);
/// Window after a cooked-mode external editor exits to absorb a co-delivered foreground-pgrp SIGINT
/// that arrived at the signal pipe after `child.wait()` resolved.
const EDITOR_SIGINT_GRACE: Duration = Duration::from_millis(250);

pub(super) fn loop_now() -> Instant {
    tokio::time::Instant::now().into_std()
}

fn render_frame(
    app: &mut App,
    frame_log: &mut Option<std::fs::File>,
    frame_log_t0: Instant,
    sched: &mut mare_tui::schedule::Schedule,
    dirty: &mut bool,
    urgent: &mut bool,
) {
    let t0 = loop_now();
    app.render();
    let took = loop_now().saturating_duration_since(t0);
    if let Some(f) = frame_log.as_mut() {
        let at_us = t0.saturating_duration_since(frame_log_t0).as_micros();
        let working = u8::from(app.working.is_some());
        let _ = writeln!(f, "{} {at_us} {working}", took.as_micros());
    }
    if took > mare_tui::schedule::FRAME {
        // slow-frame ring for /profile
        mare_shared::profile::slow_frame(took, app.frame_summary());
    }
    sched.drawn(t0);
    // captured output beyond this frame's 8 lines, or queued smooth-drain lines from a streaming
    // block, schedule the next frame (paced by `sched.frame_delay` at 16 ms/frame)
    *dirty = mare_tui::capture::pending() > 0 || app.stream_commit_pending;
    *urgent = false;
}

fn on_first_frame(
    app: &mut App,
    host: &Arc<Host>,
    ui: &Arc<UiShared>,
    had_boot_frame: bool,
    events: &mut Option<EventSource>,
    startuptime: &Option<PathBuf>,
) -> (AskpassGuard, Option<tokio::task::JoinHandle<ProbeReply>>) {
    app.reflow.initial(app.width);
    app.warn_key_conflicts();
    mare_shared::profile::mark(if had_boot_frame { "application frame" } else { "first frame" });
    // before any tool process can start: sudo prompts come back to the TUI's
    // masked input box; other sessions can reach this one
    #[cfg(unix)]
    let askpass = crate::askpass::serve(host.clone()).map_err(|e| tracing::warn!("askpass unavailable: {e}")).ok();
    #[cfg(not(unix))]
    let askpass = ();
    crate::start_peer(host);
    mare_shared::profile::mark("askpass + peer socket");
    // Trust and session restoration complete before dispatching buffered input.
    let h = host.clone();
    let ready_tx = app.app_tx.clone();
    tokio::task::spawn_local(async move {
        h.restore_startup_gc();
        if let Err(error) = settle_and_ask_trust(&h).await {
            let _ = ready_tx.send(AppMsg::StartupFailed(format!("{error:#}"), StartupFailure::Trust));
            return;
        }
        h.emit_json("app/ready", serde_json::json!({})).await;
        if let Some(error) = crate::headless::resume_error(&h).await {
            let _ = ready_tx.send(AppMsg::StartupFailed(error, StartupFailure::Resume));
            return;
        }
        let _ = ready_tx.send(AppMsg::StartupReady);
        tokio::task::yield_now().await;
        // ui/ready once the first frame is drawn (a startup block that lifts later sends it then)
        crate::app::startup::gate::finish_startup(&h).await;
    });
    let _ = mare_binding::blocking::spawn(host.lua(), || {
        if let Err(e) = mare_plugin::runtime::ensure() {
            tracing::warn!("runtime dir: {e}");
        }
    });
    tokio::task::spawn_blocking(mare_tui::highlight::preload);
    let mut tty_events = match events.take() {
        Some(EventSource::Tty(e)) => Some(e),
        other => {
            *events = other;
            None
        }
    };
    let bg_query = start_probe(app, &mut tty_events);
    if let Some(e) = tty_events {
        *events = Some(EventSource::Tty(e));
    }
    if let Some(path) = startuptime.clone() {
        let exit_after = std::env::var_os("MARE_PROFILE_STARTUP_EXIT").is_some();
        let tx = ui.tx.read().unwrap().clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            mare_shared::profile::disable_lua_sampling();
            let _ = std::fs::write(&path, mare_shared::profile::render());
            // `mare --profile-startup`: measured, now leave
            if exit_after && let Some(tx) = tx {
                let _ = tx.send(UiCmd::Exit);
            }
        });
    }
    (askpass, bg_query)
}

pub(crate) enum EventSource {
    Tty(tty::Events),
    #[allow(dead_code)]
    Channel {
        rx: mpsc::Receiver<tty::TimedEvent>,
        retained: std::collections::VecDeque<tty::TimedEvent>,
    },
}

impl EventSource {
    fn pause(&mut self) {
        match self {
            Self::Tty(e) => e.pause(),
            Self::Channel { rx, retained } => {
                // Mirror tty::Events::pause: discard pre-read key/paste/mouse events across handoff
                // while retaining terminal state events (focus/resize).
                retained.retain(|te| tty::is_preserved_across_handoff(&te.event));
                while let Ok(te) = rx.try_recv() {
                    if tty::is_preserved_across_handoff(&te.event) {
                        retained.push_back(te);
                    }
                }
            }
        }
    }

    fn resume(&mut self) {
        if let Self::Tty(e) = self {
            e.resume();
        }
    }

    async fn next(&mut self) -> Option<std::io::Result<tty::TimedEvent>> {
        match self {
            Self::Tty(e) => e.next().await,
            Self::Channel { rx, retained } => {
                if let Some(ev) = retained.pop_front() {
                    return Some(Ok(ev));
                }
                rx.recv().await.map(Ok)
            }
        }
    }

    fn try_next(&mut self) -> Option<Option<std::io::Result<tty::TimedEvent>>> {
        match self {
            Self::Tty(e) => e.try_next().map(Some),
            Self::Channel { rx, retained } => {
                if let Some(ev) = retained.pop_front() {
                    return Some(Some(Ok(ev)));
                }
                match rx.try_recv() {
                    Ok(ev) => Some(Some(Ok(ev))),
                    Err(mpsc::error::TryRecvError::Empty) => None,
                    Err(mpsc::error::TryRecvError::Disconnected) => Some(None),
                }
            }
        }
    }
}

pub(crate) struct LoopState {
    events: Option<EventSource>,
    #[cfg(unix)]
    suspend_signal: Option<tokio::signal::unix::Signal>,
    esc_join: mare_tui::term_input::EscJoin,
    bg_query: Option<tokio::task::JoinHandle<ProbeReply>>,
    /// Single source of truth for cadence (`last_anim_tick`, `last_activity`, `focused`);
    /// no derived absolute `anim_at` is stored — every `step_loop` iteration derives the next
    /// deadline from current state via `sched.next_anim_deadline(...)`.
    sched: mare_tui::schedule::Schedule,
    urgent: bool,
    dirty: bool,
    ready_sent: bool,
    gc_done: bool,
    frame_log: Option<std::fs::File>,
    frame_log_t0: Instant,
    askpass: AskpassGuard,
    had_boot_frame: bool,
    startuptime: Option<PathBuf>,
    shutdown_code: Option<i32>,
    editor_sigint_grace: Option<Instant>,
}

impl LoopState {
    pub(crate) fn for_test(event_rx: mpsc::Receiver<tty::TimedEvent>) -> Self {
        mare_tui::render::set_clock(loop_now);
        let now = loop_now();
        Self {
            events: Some(EventSource::Channel { rx: event_rx, retained: std::collections::VecDeque::new() }),
            #[cfg(unix)]
            suspend_signal: None,
            esc_join: mare_tui::term_input::EscJoin::default(),
            bg_query: None,
            sched: mare_tui::schedule::Schedule::new(now),
            urgent: false,
            dirty: false,
            ready_sent: true,
            gc_done: false,
            frame_log: None,
            frame_log_t0: now,
            askpass: Default::default(),
            had_boot_frame: false,
            startuptime: None,
            shutdown_code: None,
            editor_sigint_grace: None,
        }
    }
}

fn drain_tty_events(
    app: &mut App,
    guard: Option<&TermGuard>,
    state: &mut LoopState,
    first_ev: Option<std::io::Result<tty::TimedEvent>>,
) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let mut ev = first_ev;
    // handle every input event that is already available before the next frame,
    // so a burst of keys costs one redraw, not one per key
    loop {
        match ev {
            Some(Ok(tty::TimedEvent { event: Event::Key(k), at })) => {
                let now = loop_now();
                // Receiving keyboard input proves the terminal has focus.
                app.ui.focused.store(true, Relaxed);
                app.ui.focus_known.store(true, Relaxed);
                state.sched.set_focused(true);
                state.sched.activity(now);
                state.urgent = true;
                state.gc_done = false;
                let joined = state.esc_join.push_with_replies(k, at);
                for reply in joined.reply_bytes {
                    super::term::probe::apply_inband_reply_bytes(app, guard, &reply);
                }
                for k in joined.keys {
                    app.handle_key_at(k, at);
                }
            }
            // Late or re-probed terminal capability / OSC 11 / CPR replies route in-band.
            // Consume CPR replies once via `handle_cpr_reply` so a rejected stale reply cannot
            // fall through to `apply_inband_escape` and unconditionally re-anchor the live area.
            Some(Ok(tty::TimedEvent { event: reply, .. })) if reply.is_escape() => {
                if let Some(row) = reply.cursor_position_row() {
                    app.handle_cpr_reply(row);
                } else {
                    super::term::probe::apply_inband_escape(app, guard, &reply);
                }
            }
            Some(Ok(tty::TimedEvent { event: Event::FocusIn, .. })) => {
                let now = loop_now();
                app.ui.focused.store(true, Relaxed);
                app.ui.focus_known.store(true, Relaxed);
                state.sched.set_focused(true);
                state.sched.activity(now);
                state.urgent = true;
                state.dirty = true;
                state.gc_done = false;
                super::term::probe::maybe_reprobe_inband(app);
            }
            Some(Ok(tty::TimedEvent { event: Event::FocusOut, .. })) => {
                app.ui.focused.store(false, Relaxed);
                app.ui.focus_known.store(true, Relaxed);
                state.sched.set_focused(false);
                state.dirty = true;
            }
            Some(Ok(tty::TimedEvent { event: Event::Paste(s), at })) => {
                let now = loop_now();
                app.ui.focused.store(true, Relaxed);
                app.ui.focus_known.store(true, Relaxed);
                state.sched.set_focused(true);
                state.sched.activity(now);
                state.urgent = true;
                state.gc_done = false;
                for k in state.esc_join.flush() {
                    app.handle_key_at(k, at);
                }
                app.handle_paste_at(&s, at);
            }
            Some(Ok(tty::TimedEvent { event: Event::Mouse(m), at })) => {
                let now = loop_now();
                app.ui.focused.store(true, Relaxed);
                app.ui.focus_known.store(true, Relaxed);
                state.sched.set_focused(true);
                state.sched.activity(now);
                state.urgent = true;
                state.gc_done = false;
                for k in state.esc_join.flush() {
                    app.handle_key_at(k, at);
                }
                app.handle_mouse(m);
            }
            Some(Ok(tty::TimedEvent { event: Event::WindowResized(size), at })) => {
                let now = loop_now();
                state.sched.activity(now);
                state.urgent = true;
                state.gc_done = false;
                for k in state.esc_join.flush() {
                    app.handle_key_at(k, at);
                }
                handle_event(app, &app.ui.clone(), Event::WindowResized(size));
            }
            Some(Ok(tty::TimedEvent { event: other, at })) => {
                for k in state.esc_join.flush() {
                    app.handle_key_at(k, at);
                }
                handle_event(app, &app.ui.clone(), other);
            }
            // EOF or EIO on stdin: the terminal is gone (a hangup without SIGHUP) —
            // quit as on SIGHUP
            Some(Err(_)) | None => return true,
        }
        if app.quit || app.suspend_requested || app.external_editor_requested || app.external_editor.is_some() {
            break;
        }
        let Some(es) = state.events.as_mut() else { break };
        match es.try_next() {
            Some(next) => ev = next,
            None => break,
        }
    }
    false
}

pub(in crate::app) fn handle_app_msg(app: &mut App, msg: AppMsg) {
    match msg {
        AppMsg::StartupReady => app.startup_ready(),
        AppMsg::StartupFailed(error, kind) => app.startup_failed(error, kind),
        AppMsg::StartupRecovered => app.startup_recovered(),
        AppMsg::SetEditor(text) => app.apply_set_editor(text),
        AppMsg::Caps(caps) => app.apply_caps(caps),
        AppMsg::CommitReady(id, lines) => app.commit_ready(id, lines),
        AppMsg::Completions(seq, items, hint) => app.apply_completions(seq, items, hint),
        AppMsg::CopyResult { chars, bytes, result } => app.handle_copy_result(chars, bytes, result),
        AppMsg::ClipboardPasteReady { target, report_error, result } => {
            app.handle_clipboard_paste_result(target, report_error, result);
        }
    }
}

async fn shutdown(app: &mut App, host: &Arc<Host>, guard: TermGuard) {
    app.abort_external_editor().await;
    if app.transcript.is_open() {
        app.close_transcript();
    }
    // final: commit everything still live so the scrollback is complete
    for b in &mut app.blocks {
        b.finished = true;
    }
    app.working = None;
    app.renderer.erase();
    {
        let theme = app.theme();
        let w = app.width;
        let mut commit: Vec<Line> =
            mare_tui::capture::take_all().iter().map(|t| mare_shared::ansi::sgr_line(t, theme.fg("dim"))).collect();
        commit.extend(app.drain_held());
        while app.committed < app.blocks.len() {
            if app.committed > 0 {
                commit.push(Line::empty());
            }
            let idx = app.committed;
            let partial = app.blocks[idx].partial;
            commit.extend(
                app.blocks[idx].render(w, &theme, 0, false, app.show_reasoning, None).into_iter().skip(partial),
            );
            app.committed += 1;
        }
        app.renderer.draw(&commit, &[], None);
    }
    let _ = host.abort().await;
    let hint = resume_hint(host).await;
    drop(guard);
    let h2 = host.clone();
    crate::signals::set_shutdown_stage(crate::signals::ShutdownStage::HostShutdown);
    h2.shutdown().await;
    crate::signals::set_shutdown_stage(crate::signals::ShutdownStage::RuntimeDrain);
    if let Some(h) = hint {
        println!("{h}");
    }
}

#[cfg(unix)]
fn suspend_with_default(app: &mut App, guard: &TermGuard) -> std::io::Result<()> {
    // Tokio keeps its process-level signal handler registered even after its stream is dropped.
    // Save the complete action, use the default for suspend's raise, then restore the handler.
    // SAFETY: zero is a valid initial value for sigaction and both pointers below are valid.
    let mut default: libc::sigaction = unsafe { std::mem::zeroed() };
    let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
    default.sa_sigaction = libc::SIG_DFL;
    // SAFETY: default.sa_mask is initialized before installing the action.
    if unsafe { libc::sigemptyset(&mut default.sa_mask) } != 0
        || unsafe { libc::sigaction(libc::SIGTSTP, &default, &mut previous) } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    suspend(app, guard);
    // SAFETY: previous was populated by the successful sigaction call above.
    if unsafe { libc::sigaction(libc::SIGTSTP, &previous, std::ptr::null_mut()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn suspend_while_handed_off(app: &mut App) -> std::io::Result<()> {
    // External editor already owns the tty (cooked mode, TUI modes off, input paused).
    // Stop mare with default SIGTSTP so the whole foreground job suspends cleanly on Ctrl+Z,
    // and keep tty ownership with the editor when resumed via `fg`.
    let mut default: libc::sigaction = unsafe { std::mem::zeroed() };
    let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
    default.sa_sigaction = libc::SIG_DFL;
    // SAFETY: default.sa_mask is initialized before installing the action.
    if unsafe { libc::sigemptyset(&mut default.sa_mask) } != 0
        || unsafe { libc::sigaction(libc::SIGTSTP, &default, &mut previous) } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let resumed_fg = super::term::term_guard::suspend_until_foreground();
    // SAFETY: previous was populated by the successful sigaction call above.
    if unsafe { libc::sigaction(libc::SIGTSTP, &previous, std::ptr::null_mut()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if !resumed_fg {
        app.quit = true;
    }
    Ok(())
}

/// Single step of the production event loop: renders a pending frame when the schedule allows,
/// then awaits the next terminal/command/animation event. Returns `true` when the loop should exit.
pub(crate) async fn step_loop(
    app: &mut App,
    guard: Option<&TermGuard>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiCmd>,
    app_rx: &mut mpsc::UnboundedReceiver<AppMsg>,
    mut terminate: Pin<&mut impl Future<Output = ()>>,
    interrupts: &mut crate::signals::Interrupts,
    state: &mut LoopState,
) -> bool {
    let host = app.host.clone();
    let ui = app.ui.clone();
    // frames wait while the startup probe or an external editor owns the terminal
    if state.dirty
        && state.bg_query.is_none()
        && app.external_editor.is_none()
        && state.sched.can_draw(loop_now(), state.urgent, mare_tui::out::busy())
    {
        render_frame(
            app,
            &mut state.frame_log,
            state.frame_log_t0,
            &mut state.sched,
            &mut state.dirty,
            &mut state.urgent,
        );
        if !state.ready_sent {
            // first frame drawn: startup-to-input ends here
            state.ready_sent = true;
            let (askpass, query) =
                on_first_frame(app, &host, &ui, state.had_boot_frame, &mut state.events, &state.startuptime);
            state.askpass = askpass;
            state.bg_query = query;
        }
    }
    if app.quit {
        crate::signals::begin_forceable_shutdown(state.shutdown_code.unwrap_or(0));
        return true;
    }
    if app.suspend_requested {
        app.suspend_requested = false;
        state.esc_join = mare_tui::term_input::EscJoin::default();
        if let Some(es) = state.events.as_mut() {
            es.pause();
        }
        if let Some(guard) = guard {
            #[cfg(unix)]
            if let Err(error) = suspend_with_default(app, guard) {
                tracing::error!(%error, "failed to suspend or restore SIGTSTP handler");
                app.quit = true;
                state.shutdown_code = Some(1);
            }
            #[cfg(not(unix))]
            suspend(app, guard);
        }
        if let Some(es) = state.events.as_mut() {
            es.resume();
        }
        let now = loop_now();
        state.sched.activity(now);
        state.gc_done = false;
        state.dirty = true;
        return false;
    }
    if app.external_editor_requested {
        app.external_editor_requested = false;
        if app.external_editor.is_none() {
            state.esc_join = mare_tui::term_input::EscJoin::default();
            if let Some(es) = state.events.as_mut() {
                es.pause();
            }
            app.start_external_editor();
            if app.external_editor.is_none() {
                if let Some(es) = state.events.as_mut() {
                    es.resume();
                }
                state.dirty = true;
            }
        }
        return false;
    }
    // idle housekeeping: one full Lua GC shortly after activity stops (caches are
    // bounded, so there is no hibernation — lists it as not adopted)
    let now = loop_now();
    state.sched.set_focused(ui.focused.load(std::sync::atomic::Ordering::Relaxed));
    if !state.gc_done && app.working.is_none() && state.sched.idle_for(now) > Duration::from_secs(3) {
        state.gc_done = true;
        let _ = host.lua().gc_collect();
    }
    let editor_open = app.external_editor.is_some();
    let animating = !editor_open && (app.animating() || !app.notices.is_empty());
    let anim_at = if animating {
        if !state.sched.has_anim_tick() {
            state.sched.anchor_anim_tick(now);
        }
        state.sched.next_anim_deadline(now, app.working.is_some(), app.reduce_motion)
    } else {
        state.sched.clear_anim_tick();
        now
    };
    let writer_busy = mare_tui::out::busy();
    let redraw_in = state.sched.frame_delay(now);
    let reflow_next = if !editor_open && !app.transcript.is_open() { app.reflow.next() } else { None };
    let reflow_pending = reflow_next.is_some();
    let reflow_at = reflow_next.unwrap_or(now);
    let esc_at = if editor_open { None } else { state.esc_join.deadline() };
    let hold_at = image_hold_select_deadline(editor_open, state.dirty, app.image_hold_deadline());
    let delayed_prompt_at = if editor_open { None } else { app.delayed_prompt_deadline(now) };
    tokio::select! {
        // Suspend takes priority; otherwise typing must not wait behind streaming output.
        biased;
        editor_status = async {
            match app.external_editor.as_mut() {
                Some(session) => session.child.wait().await,
                None => std::future::pending().await,
            }
        }, if editor_open => {
            if let Some(session) = app.external_editor.take() {
                app.finish_external_editor(session, editor_status);
            }
            if let Some(es) = state.events.as_mut() {
                es.resume();
            }
            // If Ctrl+C in a cooked-mode editor terminated the child slightly before the co-delivered
            // SIGINT reached `interrupts`, disarm the shutdown watchdog and absorb that one SIGINT.
            crate::signals::complete_turn_interrupt();
            let now = loop_now();
            state.editor_sigint_grace = Some(now + EDITOR_SIGINT_GRACE);
            state.sched.activity(now);
            state.gc_done = false;
            state.urgent = true;
            state.dirty = true;
        }
        _ = async {
            #[cfg(unix)]
            if let Some(signal) = state.suspend_signal.as_mut() {
                let _ = signal.recv().await;
                return;
            }
            std::future::pending::<()>().await;
        } => {
            if editor_open {
                #[cfg(unix)]
                if let Err(error) = suspend_while_handed_off(app) {
                    tracing::error!(%error, "failed to suspend or restore SIGTSTP handler during editor handoff");
                    app.quit = true;
                    state.shutdown_code = Some(1);
                }
            } else {
                app.suspend_requested = true;
            }
        }
        ev = async {
            match state.events.as_mut() {
                Some(e) => e.next().await,
                None => std::future::pending().await,
            }
        }, if !editor_open => {
            if drain_tty_events(app, guard, state, ev) {
                return true;
            }
            state.dirty = true;
        }
        _ = &mut terminate => {
            state.shutdown_code = Some(0);
            app.quit = true;
        }
        _ = interrupts.recv() => {
            if app.external_editor.is_some() {
                // Cooked-mode editors (e.g. `code --wait`) share the foreground process group with
                // ISIG enabled, so Ctrl+C delivers SIGINT to both the editor and mare. Cancel the
                // editor instead of exiting mare, and acknowledge the native signal so the 5s
                // force-exit watchdog does not terminate mare.
                crate::signals::complete_turn_interrupt();
                app.cancel_external_editor();
            } else if state.editor_sigint_grace.take().is_some_and(|until| loop_now() <= until) {
                crate::signals::complete_turn_interrupt();
            } else {
                state.shutdown_code = Some(130);
                app.quit = true;
            }
        }
        res = async {
            match state.bg_query.as_mut() {
                Some(j) => j.await,
                None => std::future::pending().await,
            }
        } => {
            state.bg_query = None;
            state.events = apply_probe_reply(app, &host, guard.expect("probe only runs with TermGuard"), res.unwrap_or_default())
                .map(EventSource::Tty);
            state.dirty = true;
        }
        Some(cmd) = ui_rx.recv() => {
            let now = loop_now();
            state.sched.activity(now);
            state.gc_done = false;
            let had_modal = app.modal.is_some();
            app.handle_cmd(cmd);
            while let Ok(c) = ui_rx.try_recv() {
                app.handle_cmd(c);
            }
            if !had_modal && app.modal.is_some() {
                state.urgent = true;
            }
            state.dirty = true;
        }
        Some(msg) = app_rx.recv() => {
            handle_app_msg(app, msg);
            state.dirty = true;
        }
        // Coalesce all active event-loop deadlines (resize reflow, delayed approval modal,
        // lone ESC disambiguation, animation tick, and image-placeholder hold) onto one timer.
        _ = tokio::time::sleep_until(
            [
                reflow_pending.then_some(reflow_at),
                delayed_prompt_at,
                esc_at,
                animating.then_some(anim_at),
                hold_at,
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(now)
            .into(),
        ), if reflow_pending || delayed_prompt_at.is_some() || esc_at.is_some() || animating || hold_at.is_some() => {
            let now = loop_now();
            if reflow_pending && now >= reflow_at {
                match app.reflow.take_due(now, app.width) {
                    Some(mare_tui::reflow::Due::Rebuild) => app.rebuild_history(),
                    Some(mare_tui::reflow::Due::Repaint) => app.renderer.erase(),
                    None => {}
                }
                state.dirty = true;
            }
            if delayed_prompt_at.is_some_and(|at| now >= at) && app.maybe_show_delayed_prompts(now) {
                state.urgent = true;
                state.dirty = true;
            }
            if esc_at.is_some_and(|at| now >= at) {
                for k in state.esc_join.flush() {
                    app.handle_key(k);
                }
                state.urgent = true;
                state.dirty = true;
            }
            if animating && now >= anim_at {
                let lateness = now.saturating_duration_since(anim_at);
                if lateness > ANIM_LATE_WARN {
                    mare_shared::profile::slow_frame(lateness, format!("anim tick late: {}", app.frame_summary()));
                    tracing::debug!(lateness_ms = lateness.as_secs_f64() * 1000.0, "animation tick late");
                }
                app.frame = app.frame.wrapping_add(1);
                state.sched.note_anim_tick(now);
                state.dirty = true;
            }
            if hold_at.is_some_and(|at| now >= at) {
                state.dirty = true;
            }
        }
        _ = mare_tui::capture::arrived() => {
            state.dirty = true;
        }
        // a picture finished preprocessing: the next frame draws it in its reserved rows
        _ = mare_tui::image::ready() => {
            app.transcript.invalidate_cache();
            if mare_tui::image::take_replay_pending() {app.reflow.invalidated(loop_now());}
            state.dirty = true;
        }
        // backpressure: the previous frame is still being written — wait for the writer
        _ = mare_tui::out::drained(), if !editor_open && state.dirty && writer_busy => {}
        _ = tokio::time::sleep(redraw_in), if !editor_open && state.dirty && !writer_busy && state.bg_query.is_none() => {}
    }
    false
}

/// Skip arming the image-hold timer once `dirty` is already set (e.g. while waiting on writer
/// backpressure or frame throttling) so an expired deadline cannot busy-spin `tokio::select!`.
fn image_hold_select_deadline(editor_open: bool, dirty: bool, deadline: Option<Instant>) -> Option<Instant> {
    if editor_open || dirty { None } else { deadline }
}

async fn finish_boot_exit(code: i32, host: Option<Arc<Host>>) -> i32 {
    if let Some(host) = host {
        crate::signals::set_shutdown_stage(crate::signals::ShutdownStage::HostShutdown);
        host.shutdown().await;
        crate::signals::set_shutdown_stage(crate::signals::ShutdownStage::RuntimeDrain);
    }
    code
}

async fn run_loop(
    app: &mut App,
    guard: &TermGuard,
    ui_rx: &mut mpsc::UnboundedReceiver<UiCmd>,
    app_rx: &mut mpsc::UnboundedReceiver<AppMsg>,
    mut terminate: Pin<&mut impl Future<Output = ()>>,
    interrupts: &mut crate::signals::Interrupts,
    state: &mut LoopState,
) {
    while !step_loop(app, Some(guard), ui_rx, app_rx, terminate.as_mut(), interrupts, state).await {}
}

pub async fn run(
    opts: HostOptions,
    initial_prompt: Option<String>,
    startuptime: Option<PathBuf>,
    strict_config: bool,
) -> Result<i32> {
    let ui = UiShared::new();
    let (tx, mut ui_rx) = mpsc::unbounded_channel();
    *ui.tx.write().unwrap() = Some(tx);
    #[cfg(unix)]
    let suspend_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(libc::SIGTSTP))?;
    let term = init_terminal(&ui, &opts.config)?;

    let terminate = crate::signals::terminate();
    tokio::pin!(terminate);
    let mut interrupts = crate::signals::Interrupts::new();

    let (host, boot_input) = {
        let boot_interrupt = interrupts.recv();
        tokio::pin!(boot_interrupt);
        match boot_host(opts, &ui, &term, &startuptime, strict_config, terminate.as_mut(), boot_interrupt).await? {
            BootOutcome::Ready { host, boot_input } => (host, boot_input),
            BootOutcome::Exit { code, host } => {
                drop(term.guard);
                return Ok(finish_boot_exit(code, host).await);
            }
            BootOutcome::StrictConfigError { host, message } => {
                drop(term.guard);
                let _ = finish_boot_exit(2, Some(host)).await;
                eprintln!("mare: {message}");
                return Ok(2);
            }
        }
    };

    // sudo prompts come back to the TUI's masked input box
    mare_shared::profile::mark("host ready");
    // keymap comes entirely from Lua (mare.keymap.set — defaults included); later changes
    // arrive as UiCmd::SetBindings
    let (app_tx, mut app_rx) = mpsc::unbounded_channel();
    let had_boot_frame = boot_input.is_some();
    let mut app = init_app(&host, &ui, app_tx, boot_input, &term, initial_prompt);
    let guard = term.guard;

    // UI output queued while plugins loaded (welcome line, widgets, status) belongs in the very
    // first frame, not one frame-interval later
    while let Ok(c) = ui_rx.try_recv() {
        app.handle_cmd(c);
    }

    // created after the first frame: the terminal background query (OSC 11) must finish before
    // anything else reads the tty, and must not delay the first frame
    // frame scheduling: ≤ 60 fps, input renders at once, backpressure from the write exit, the
    // timed (animation) redraw slows when idle or unfocused
    // askpass (sudo prompts) and the peer socket write files and bind sockets — tens of ms on
    // macOS — so they start right after the first frame (below), not before it
    mare_tui::render::set_clock(loop_now);
    // MARE_FRAME_LOG=<file>: `<took_us> <at_us> <working>` per line, for `just bench`
    let loop_t0 = loop_now();
    let mut state = LoopState {
        events: None,
        #[cfg(unix)]
        suspend_signal: Some(suspend_signal),
        esc_join: mare_tui::term_input::EscJoin::default(),
        bg_query: None,
        sched: mare_tui::schedule::Schedule::new(loop_t0),
        urgent: false,
        dirty: true,
        ready_sent: false,
        gc_done: false,
        frame_log: std::env::var_os("MARE_FRAME_LOG").and_then(|p| std::fs::File::create(p).ok()),
        frame_log_t0: loop_t0,
        askpass: Default::default(),
        had_boot_frame,
        startuptime,
        shutdown_code: None,
        editor_sigint_grace: None,
    };

    run_loop(&mut app, &guard, &mut ui_rx, &mut app_rx, terminate.as_mut(), &mut interrupts, &mut state).await;

    #[cfg(unix)]
    drop(state.askpass);
    drop(state.events.take());
    shutdown(&mut app, &host, guard).await;
    Ok(0)
}

/// The resume line printed on exit: only for a session that is on disk — nothing for
/// `--no-session` or a session that never got a message.
pub(crate) async fn resume_hint(host: &Host) -> Option<String> {
    let s = host.service_table("session")?;
    let info = s.get::<mlua::Function>("info").ok()?;
    let mlua::Value::Table(t) = info.call_async::<mlua::Value>(s).await.ok()? else {
        return None;
    };
    let path = t.get::<Option<String>>("path").ok().flatten()?;
    if t.get::<Option<bool>>("ephemeral").ok().flatten().unwrap_or(false) || !std::path::Path::new(&path).exists() {
        return None;
    }
    let id = t.get::<Option<String>>("session_id").ok().flatten()?;
    Some(format!("Resume this session with: mare -r {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf_from_the_merged_config_sets_the_cache_caps() {
        let cache = &mare_tui::render::cache::PREVIEW;
        apply_perf(&serde_json::json!({ "perf": { "preview_cache": 1 } }));
        cache.get_or_insert(1, Vec::new);
        cache.get_or_insert(2, Vec::new);
        assert_eq!(cache.len(), 1, "preview_cache = 1 keeps one entry");
        apply_perf(&serde_json::json!({ "perf": { "preview_cache": 0 } }));
        assert_eq!(cache.len(), 0, "0 turns the cache off");
        apply_perf(&serde_json::json!({ "perf": { "preview_cache": 256 } }));
    }

    #[test]
    fn image_hold_select_deadline_suppresses_expired_timer_while_dirty() {
        let deadline = Instant::now();
        assert_eq!(image_hold_select_deadline(false, false, Some(deadline)), Some(deadline));
        assert_eq!(image_hold_select_deadline(false, true, Some(deadline)), None);
        assert_eq!(image_hold_select_deadline(true, false, Some(deadline)), None);
        assert_eq!(image_hold_select_deadline(false, false, None), None);
    }

    #[tokio::test]
    async fn finish_boot_exit_runs_host_shutdown_when_host_is_present() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                assert_eq!(finish_boot_exit(130, None).await, 130);

                let dir = tempfile::tempdir().unwrap();
                let opts = HostOptions {
                    cwd: dir.path().to_path_buf(),
                    mode: mare_plugin::Mode::Tui,
                    config: serde_json::json!({ "plugins": { "onboarding": { "plugins": false } } }),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({}),
                    no_user_plugins: true,
                };
                let ui = UiShared::new();
                let host = Host::with_ui(opts, ui).await.unwrap();
                let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let ran_cb = ran.clone();
                let cb = host
                    .lua()
                    .create_function(move |_, ()| {
                        ran_cb.store(true, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                let mare = host.lua().globals().get::<mlua::Table>("mare").unwrap();
                let on = mare.get::<mlua::Function>("on").unwrap();
                on.call::<mlua::Value>(("app/shutdown", cb)).unwrap();

                let code = finish_boot_exit(0, Some(host.clone())).await;
                assert_eq!(code, 0);
                assert!(host.cancel_token().is_cancelled(), "finish_boot_exit must cancel the host root token");
                assert!(
                    ran.load(std::sync::atomic::Ordering::SeqCst),
                    "finish_boot_exit must emit app/shutdown via host.shutdown()"
                );
            })
            .await;
    }
}

#[cfg(test)]
#[cfg(unix)]
mod suspend_pty_tests {
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    const TTY_FLAGS: libc::tcflag_t = libc::ICANON | libc::ECHO | libc::ISIG;

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn tty_flags(slave: &File) -> libc::tcflag_t {
        // SAFETY: termios is initialized by tcgetattr and slave is an open PTY descriptor.
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut term) },
            0,
            "tcgetattr: {}",
            io::Error::last_os_error()
        );
        term.c_lflag & TTY_FLAGS
    }

    fn read_pty(master: &mut File, output: &mut Vec<u8>) {
        let mut poll = libc::pollfd { fd: master.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: poll points to one valid descriptor and has a bounded timeout.
        let ready = unsafe { libc::poll(&mut poll, 1, 25) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted, "poll PTY: {error}");
        } else if ready > 0 && poll.revents & libc::POLLIN != 0 {
            let mut buf = [0; 8192];
            match master.read(&mut buf) {
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                Err(error) => panic!("read PTY: {error}"),
            }
        }
    }

    fn wait_raw_frame(child: &mut Child, master: &mut File, slave: &File) {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut output = Vec::new();
        loop {
            if tty_flags(slave) == 0 && String::from_utf8_lossy(&output).contains('╰') {
                return;
            }
            assert!(Instant::now() < deadline, "no raw TUI frame: {}", String::from_utf8_lossy(&output));
            assert!(
                child.try_wait().unwrap().is_none(),
                "mare exited before rendering: {}",
                String::from_utf8_lossy(&output)
            );
            read_pty(master, &mut output);
        }
    }

    fn wait_stopped(child: &Child, master: &mut File) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        loop {
            let mut status = 0;
            // SAFETY: the pid is our child and status is a valid out-parameter.
            let result =
                unsafe { libc::waitpid(child.id() as libc::pid_t, &mut status, libc::WNOHANG | libc::WUNTRACED) };
            assert!(result >= 0, "waitpid: {}", io::Error::last_os_error());
            if result != 0 {
                assert!(libc::WIFSTOPPED(status), "mare exited instead of stopping: {status}");
                assert_eq!(libc::WSTOPSIG(status), libc::SIGTSTP, "wrong stop signal");
                return;
            }
            assert!(Instant::now() < deadline, "mare did not stop: {}", String::from_utf8_lossy(&output));
            read_pty(master, &mut output);
        }
    }

    #[test]
    fn external_sigtstp_suspends_cooked_and_resumes_twice() {
        // Restores the terminal before SIGTSTP.
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize { ws_row: 30, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: openpty writes the two descriptors and reads the initialized winsize.
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
        // SAFETY: openpty returned two owned descriptors, each transferred exactly once.
        let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            // SAFETY: fd is valid; set close-on-exec so the child inherits only its stdio copies.
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }, 0);
        }
        assert_eq!(tty_flags(&slave), TTY_FLAGS, "PTY must start cooked");

        let test_exe = std::env::current_exe().unwrap();
        let mare = test_exe
            .ancestors()
            .map(|dir| dir.join("mare"))
            .find(|path| path.is_file())
            .unwrap_or_else(|| panic!("build the faux mare binary before this test: {}", test_exe.display()));
        let mut command = Command::new(mare);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("MARE_")
                || matches!(key.to_str(), Some("GIT_DIR" | "GIT_WORK_TREE" | "GIT_INDEX_FILE"))
            {
                command.env_remove(key);
            }
        }
        command
            .args(["--model", "faux/echo", "--no-session", "--no-trust", "--set", "plugins.onboarding.plugins=false"])
            .current_dir(work.path())
            .env("MARE_HOME", home.path())
            .env("MARE_TEST_OFFLINE", "1")
            .env("MARE_NO_TERM_PROBE", "1")
            .env("TERM", "xterm-256color")
            .process_group(0)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        // A distinct process group in the parent's session is not orphaned, so SIGTSTP can stop it.
        // SAFETY: resetting a signal disposition is async-signal-safe in the child before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::signal(libc::SIGTSTP, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = ChildGuard(command.spawn().unwrap());
        drop(command);
        wait_raw_frame(&mut child.0, &mut master, &slave);
        for _ in 0..2 {
            // SAFETY: the pid is our live child, and these signals are sent only to that child.
            assert_eq!(unsafe { libc::kill(child.0.id() as libc::pid_t, libc::SIGTSTP) }, 0);
            wait_stopped(&child.0, &mut master);
            assert_eq!(tty_flags(&slave), TTY_FLAGS, "SIGTSTP must leave the terminal cooked");
            assert_eq!(unsafe { libc::kill(child.0.id() as libc::pid_t, libc::SIGCONT) }, 0);
            wait_raw_frame(&mut child.0, &mut master, &slave);
        }
        // The in-band Ctrl+Z path must still suspend after installing the signal listener.
        master.write_all(b"\x1a").unwrap();
        wait_stopped(&child.0, &mut master);
        assert_eq!(tty_flags(&slave), TTY_FLAGS, "Ctrl+Z must leave the terminal cooked");
        // SAFETY: the pid is our stopped child.
        assert_eq!(unsafe { libc::kill(child.0.id() as libc::pid_t, libc::SIGCONT) }, 0);
        wait_raw_frame(&mut child.0, &mut master, &slave);
    }

    #[test]
    fn sigtstp_resize_emits_ui_resize_and_reflows_scrollback() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let resize_log = work.path().join("resize.log");
        let init_lua = format!(
            r#"
mare.on("app/ready", function()
    mare.ui.print(mare.ui.Paragraph(string.rep("word ", 20) .. "ENDMARK"))
end)
mare.on("ui/resize", function(ev)
    local f = io.open({:?}, "a")
    if f then
        f:write(tostring(ev.width) .. "x" .. tostring(ev.height) .. "\n")
        f:close()
    end
end)
"#,
            resize_log.to_string_lossy()
        );
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(home.path().join("config").join("init.lua"), init_lua).unwrap();

        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize { ws_row: 30, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: openpty writes the two descriptors and reads the initialized winsize.
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
        // SAFETY: openpty returned two owned descriptors, each transferred exactly once.
        let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            // SAFETY: fd is valid; set close-on-exec so the child inherits only its stdio copies.
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }, 0);
        }

        let test_exe = std::env::current_exe().unwrap();
        let mare = test_exe
            .ancestors()
            .map(|dir| dir.join("mare"))
            .find(|path| path.is_file())
            .unwrap_or_else(|| panic!("build the faux mare binary before this test: {}", test_exe.display()));
        let mut command = Command::new(mare);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("MARE_")
                || matches!(key.to_str(), Some("GIT_DIR" | "GIT_WORK_TREE" | "GIT_INDEX_FILE"))
            {
                command.env_remove(key);
            }
        }
        command
            .args(["--model", "faux/echo", "--no-session", "--no-trust", "--set", "plugins.onboarding.plugins=false"])
            .current_dir(work.path())
            .env("MARE_HOME", home.path())
            .env("MARE_TEST_OFFLINE", "1")
            .env("MARE_NO_TERM_PROBE", "1")
            .env("TERM", "xterm-256color")
            .process_group(0)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        // SAFETY: resetting a signal disposition is async-signal-safe in the child before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::signal(libc::SIGTSTP, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = ChildGuard(command.spawn().unwrap());
        drop(command);
        wait_raw_frame(&mut child.0, &mut master, &slave);

        // Wait until the initial printed paragraph has been emitted.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while !String::from_utf8_lossy(&seen).contains("ENDMARK") {
            assert!(Instant::now() < deadline, "initial paragraph never printed: {}", String::from_utf8_lossy(&seen));
            read_pty(&mut master, &mut seen);
        }

        // Stop the process, resize the PTY while suspended, then resume.
        // SAFETY: the pid is our live child.
        assert_eq!(unsafe { libc::kill(child.0.id() as libc::pid_t, libc::SIGTSTP) }, 0);
        wait_stopped(&child.0, &mut master);

        let narrow = libc::winsize { ws_row: 30, ws_col: 60, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: master is a valid PTY fd and narrow is a valid winsize pointer.
        assert_eq!(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &narrow) }, 0);

        // SAFETY: the pid is our stopped child.
        assert_eq!(unsafe { libc::kill(child.0.id() as libc::pid_t, libc::SIGCONT) }, 0);
        wait_raw_frame(&mut child.0, &mut master, &slave);

        // After resuming at 60x30, `ui/resize` must fire and reflow must rebuild the scrollback (re-emitting ENDMARK).
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut post_resume = Vec::new();
        let mut saw_resize_log = false;
        let mut saw_reflow = false;
        while !(saw_resize_log && saw_reflow) {
            if let Ok(log) = std::fs::read_to_string(&resize_log)
                && log.lines().any(|line| line == "60x30")
            {
                saw_resize_log = true;
            }
            read_pty(&mut master, &mut post_resume);
            if String::from_utf8_lossy(&post_resume).contains("ENDMARK") {
                saw_reflow = true;
            }
            if Instant::now() >= deadline {
                panic!(
                    "resume after resize while suspended failed (saw_resize_log={saw_resize_log}, saw_reflow={saw_reflow}); log={:?}",
                    std::fs::read_to_string(&resize_log).unwrap_or_default()
                );
            }
        }
    }
}
