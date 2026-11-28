use super::*;

pub struct Vt {
    app: App,
    ui: Arc<UiShared>,
    ui_rx: mpsc::UnboundedReceiver<UiCmd>,
    app_rx: mpsc::UnboundedReceiver<AppMsg>,
    pub host: Arc<Host>,
    started: bool,
    startup: Option<tokio::task::JoinHandle<()>>,
    taps: mare_kernel::EventCursor,
    turns_ended: usize,
    loop_state: Option<Box<super::event_loop::LoopState>>,
    event_tx: Option<mpsc::Sender<tty::TimedEvent>>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Vt {
    /// Start the host (plugins, init.lua) and the app for a `w`×`h` terminal whose cursor
    /// sits at the top-left (the live area is anchored there, so commits go through the
    /// scroll region like on a real terminal that answered the startup probe).
    // the guard serializes tests that share the process-global write exit; each
    // #[tokio::test] has its own runtime thread, so waiting on it blocks nothing else
    #[allow(clippy::await_holding_lock)]
    pub async fn new(opts: HostOptions, w: u16, h: u16) -> Result<Self> {
        mare_tui::render::set_clock(super::event_loop::loop_now);
        // the write exit and render state are process-global
        let lock = mare_tui::out::test_lock();
        let _ = mare_tui::out::capture(true);
        let ui = UiShared::new();
        let (tx, ui_rx) = mpsc::unbounded_channel();
        *ui.tx.write().unwrap() = Some(tx);
        ui.width.store(w, std::sync::atomic::Ordering::Relaxed);
        ui.height.store(h, std::sync::atomic::Ordering::Relaxed);
        let host = Host::with_ui(opts, ui.clone()).await?;
        let taps = host.kernel.open_cursor(Arc::new(|_| Some(true)));
        let (app_tx, app_rx) = mpsc::unbounded_channel();
        let mut app = App::new(host.clone(), ui.clone(), app_tx, w, h);
        app.renderer.anchor(0);
        // keys arrive here at machine speed but are typed keys, never a paste (pastes go
        // through `paste`): the unbracketed-paste heuristic (on by default on Windows) would
        // read every typed line + Enter as a pasted newline. Its timing has its own tests
        // (`mare_tui::paste`)
        app.burst_enabled = false;
        Ok(Self {
            app,
            ui,
            ui_rx,
            app_rx,
            host,
            started: false,
            startup: None,
            taps,
            turns_ended: 0,
            loop_state: None,
            event_tx: None,
            _lock: lock,
        })
    }

    /// Drain initial startup UI commands and draw the first frame without emitting `app/ready` or
    /// `ui/ready` yet. Lets tests inspect the exact pre-ready first frame and compare it against
    /// the settled post-ready frame.
    pub async fn first_frame(&mut self) {
        for _ in 0..4 {
            self.drain();
            tokio::task::yield_now().await;
        }
        self.app.render();
    }

    /// Run queued UI commands and app messages, let Lua tasks progress, and draw one frame.
    /// The first call also does what `run` does after its first frame (`app/ready`, caps).
    pub async fn frame(&mut self) {
        for _ in 0..4 {
            self.drain();
            tokio::task::yield_now().await;
        }
        self.app.maybe_show_delayed_prompts(app_now());
        if self.app.external_editor.is_none() {
            self.app.render();
        }
        if !self.started {
            self.started = true;
            self.app.reflow.initial(self.app.width);
            self.app.publish_caps(&Default::default());
            let h = self.host.clone();
            let tx = self.app.app_tx.clone();
            self.startup = Some(tokio::task::spawn_local(async move {
                h.restore_startup_gc();
                if let Err(error) = settle_and_ask_trust(&h).await {
                    let _ = tx.send(AppMsg::StartupFailed(format!("{error:#}"), StartupFailure::Trust));
                    return;
                }
                h.emit_json("app/ready", serde_json::json!({})).await;
                if let Some(error) = crate::headless::resume_error(&h).await {
                    let _ = tx.send(AppMsg::StartupFailed(error, StartupFailure::Resume));
                    return;
                }
                let _ = tx.send(AppMsg::StartupReady);
                // as the TUI does (event_loop.rs)
                crate::app::startup::gate::finish_startup(&h).await;
            }));
            for _ in 0..4 {
                tokio::task::yield_now().await;
                self.drain();
            }
            self.drain();
            self.app.render();
        }
    }

    /// End the virtual app's producers and drain the Host before its files are released.
    pub async fn shutdown(&mut self) {
        if let Some(startup) = self.startup.take() {
            startup.abort();
            let _ = startup.await;
        }
        let _ = self.host.abort().await;
        self.host.shutdown().await;
    }

    /// How many agent runs have ended (`turn/completed`) so far.
    pub fn turns_ended(&mut self) -> usize {
        while let Ok(Some(rec)) = self.taps.try_recv() {
            if let mare_kernel::CursorRecord::Event { event, .. } = rec
                && event.name == "turn/completed"
            {
                self.turns_ended += 1;
            }
        }
        self.turns_ended
    }

    /// Drain queued `UiCmd` and `AppMsg` events without rendering a frame.
    pub fn drain_pending(&mut self) {
        self.drain();
    }

    /// Whether a modal is currently open.
    pub fn has_modal(&self) -> bool {
        self.app.modal.is_some()
    }

    fn drain(&mut self) {
        if self.app.external_editor_requested {
            self.app.external_editor_requested = false;
            self.app.start_external_editor();
        }
        self.app.poll_external_editor_nonblocking();
        while let Ok(c) = self.ui_rx.try_recv() {
            self.app.handle_cmd(c);
        }
        while let Ok(msg) = self.app_rx.try_recv() {
            super::event_loop::handle_app_msg(&mut self.app, msg);
        }
    }

    /// Bytes written to the terminal since the last call.
    pub fn output(&mut self) -> Vec<u8> {
        mare_tui::out::capture(true)
    }

    pub fn key(&mut self, k: KeyEvent) {
        self.app.handle_key(k);
    }

    pub fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            let code = if c == '\n' { KeyCode::Enter } else { KeyCode::Char(c) };
            self.app.handle_key(KeyEvent::new(code, Modifiers::NONE));
        }
    }

    pub fn paste(&mut self, s: &str) {
        handle_event(&mut self.app, &self.ui, Event::Paste(s.to_string()));
    }

    pub fn focus(&mut self, focused: bool) {
        let ev = if focused { Event::FocusIn } else { Event::FocusOut };
        if self.loop_state.is_some() {
            self.send_tty_event(ev);
        } else {
            handle_event(&mut self.app, &self.ui, ev);
        }
    }

    pub fn scroll_insert_ready(&self) -> bool {
        self.app.renderer.scroll_insert_ready()
    }

    pub fn anchor(&mut self, row: usize) {
        self.app.renderer.anchor(row);
        self.app.cpr_pending = false;
        self.app.cpr_stale_replies = 0;
        self.app.sync_position_known();
    }

    pub fn send_cpr(&mut self, line: u16, col: u16) {
        let ev = Event::cursor_position_report(line, col);
        handle_event(&mut self.app, &self.ui, ev);
    }

    /// Inject a terminal event into the production `step_loop` (`EventSource::Channel`),
    /// stamped with the current injectable clock (`mare_tui::render::clock_now()`).
    pub fn send_tty_event(&mut self, event: Event) {
        self.send_tty_event_at(event, mare_tui::render::clock_now());
    }

    /// Inject a terminal event with an explicit arrival timestamp (for testing stalled drain
    /// batches against `PasteGuard` / `EscJoin` / `PasteBurst`).
    pub fn send_tty_event_at(&mut self, event: Event, at: Instant) {
        self.ensure_loop_state();
        if let Some(tx) = self.event_tx.as_ref() {
            let _ = tx.try_send(tty::TimedEvent { event, at });
        }
    }

    fn ensure_loop_state(&mut self) {
        if self.loop_state.is_none() {
            let (tx, rx) = mpsc::channel(256);
            self.event_tx = Some(tx);
            self.loop_state = Some(Box::new(super::event_loop::LoopState::for_test(rx)));
        }
    }

    /// Resize and run the debounced reflow to completion at once (the debounce itself is
    /// covered by `mare_tui::reflow`'s own tests).
    pub fn resize(&mut self, w: u16, h: u16) {
        self.resize_with_pixels(w, h, None, None);
    }

    /// Resize with optional terminal pixel dimensions (e.g. font zoom or cross-DPI move)
    /// and run the debounced reflow to completion.
    pub fn resize_with_pixels(&mut self, w: u16, h: u16, pixel_width: Option<u16>, pixel_height: Option<u16>) {
        let size = mare_tui::tty::WindowSize { cols: w, rows: h, pixel_width, pixel_height };
        handle_event(&mut self.app, &self.ui, Event::WindowResized(size));
        let later = app_now() + Duration::from_secs(5);
        while let Some(due) = self.app.reflow.take_due(later, self.app.width) {
            match due {
                mare_tui::reflow::Due::Rebuild => self.app.rebuild_history_from_reflow(),
                mare_tui::reflow::Due::Repaint => self.app.renderer.erase(),
            }
        }
    }

    /// Startup (trust, `app/ready`, the -c / --resume session) hasn't finished yet.
    pub fn starting(&self) -> bool {
        self.app.startup.starting
    }

    /// Startup (`app/ready`, `StartupReady`, `ui/ready`) has fully completed.
    pub fn startup_finished(&self) -> bool {
        self.started && !self.app.startup.starting && self.startup.as_ref().is_none_or(|task| task.is_finished())
    }

    /// The editor's current text.
    pub fn editor_text(&self) -> String {
        self.app.ed().raw_text().clone()
    }

    pub fn quit_requested(&self) -> bool {
        self.app.quit
    }

    pub fn busy(&self) -> bool {
        self.app.working.is_some()
    }

    /// Whether the app currently schedules timed animation redraws.
    pub fn animating(&self) -> bool {
        self.app.animating()
    }

    /// Current animation frame counter.
    pub fn anim_frame_count(&self) -> usize {
        self.app.frame
    }

    /// Number of `UiCmd::Clear` commands processed by the app so far.
    pub fn clear_count(&self) -> usize {
        self.app.clear_count
    }

    /// Whether the `Ctrl+T` or plugin transcript overlay is currently open.
    pub fn transcript_open(&self) -> bool {
        self.app.transcript.is_open()
    }

    /// Whether `expand_all` (`Ctrl+O`) is currently enabled.
    pub fn expand_all(&self) -> bool {
        self.app.expand_all
    }

    /// Wait for the background startup task (`app/ready`, `StartupReady`, `finish_startup`) to
    /// complete, drain any queued startup messages/commands, and flush the writer thread before
    /// pausing tokio time for deterministic event-loop tests.
    pub async fn wait_startup_settled(&mut self) {
        if let Some(startup) = self.startup.take() {
            let _ = startup.await;
        }
        self.drain();
        tokio::task::yield_now().await;
        self.drain();
        self.app.render();
        mare_tui::out::drained().await;
        let _ = self.output();
        // Discard any startup CPR query emitted before the test began capturing output.
        self.app.cpr_pending = false;
        self.app.cpr_stale_replies = 0;
    }

    /// Initialize (or reset) the production `LoopState` bound to the current tokio clock.
    pub fn reset_loop_state(&mut self) {
        self.loop_state = None;
        self.event_tx = None;
        self.ensure_loop_state();
    }

    /// Run the production `event_loop::step_loop` against the virtual app until `for_duration` elapses
    /// or the loop exits (intended for `tokio::time::pause()` deterministic timer tests).
    /// Waits for the OS tty-writer thread (`mare_tui::out::drained()`) outside the `sleep_until`
    /// select so tokio's paused clock never auto-advances past `for_duration` while a rendered
    /// frame is still being flushed by the background writer thread.
    pub async fn run_production_loop_for(&mut self, for_duration: Duration) {
        let _ = self.run_production_loop_until(for_duration, |_| false).await;
    }

    /// Run the production `event_loop::step_loop` until `pred(self)` returns `true`, the loop
    /// exits, or `max_duration` elapses on the tokio clock. Returns `true` when `pred` matched.
    pub async fn run_production_loop_until(
        &mut self,
        max_duration: Duration,
        mut pred: impl FnMut(&mut Self) -> bool,
    ) -> bool {
        self.ensure_loop_state();
        let deadline = tokio::time::Instant::now() + max_duration;
        let terminate = std::future::pending::<()>();
        tokio::pin!(terminate);
        let mut interrupts = crate::signals::Interrupts::listen(false);
        loop {
            // Wait for the OS writer thread without parking the tokio runtime, so paused virtual
            // time never auto-advances past `max_duration` while a captured frame is being copied.
            while mare_tui::out::busy() {
                std::thread::yield_now();
            }
            if pred(self) {
                return true;
            }
            let mut state = self.loop_state.take().expect("loop_state initialized");
            let mut timeout = std::pin::pin!(tokio::time::sleep_until(deadline));
            let stopped = tokio::select! {
                biased;
                done = super::event_loop::step_loop(
                    &mut self.app,
                    None,
                    &mut self.ui_rx,
                    &mut self.app_rx,
                    terminate.as_mut(),
                    &mut interrupts,
                    &mut state,
                ) => done,
                _ = &mut timeout => true,
            };
            self.loop_state = Some(state);
            while mare_tui::out::busy() {
                std::thread::yield_now();
            }
            if pred(self) {
                return true;
            }
            if stopped {
                return false;
            }
        }
    }

    /// Transcript blocks still in the live area (not yet committed to the scrollback).
    pub fn live_blocks(&self) -> usize {
        self.app.blocks.len() - self.app.committed
    }

    /// The markdown source received so far by the live (not yet committed) blocks: what a
    /// streaming answer holds, whether or not a notice card covers it on screen.
    pub fn live_source(&self) -> String {
        self.app.blocks[self.app.committed..].iter().filter_map(|b| b.source()).collect()
    }

    /// The line `run` prints on exit (`Resume this session with: mare -r <id>`), if any.
    pub async fn resume_hint(&self) -> Option<String> {
        super::resume_hint(&self.host).await
    }

    /// Paste-burst detection on or off (the constructor turns it off: keys arrive here at
    /// machine speed, which is paste speed).
    pub fn set_paste_burst(&mut self, on: bool) {
        self.app.burst_enabled = on;
    }

    /// Drop the time-based notice cards (they expire on a timer; snapshots must not depend
    /// on how long a frame took).
    pub fn dismiss_notices(&mut self) {
        self.app.notices.clear();
    }

    /// Push a notice pre-aged by `age` (for deterministic expiry/deferral tests without sleeps).
    pub fn push_aged_notice(&mut self, msg: &str, age: Duration) {
        let at = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        self.app.notices.push((msg.into(), "info".into(), at));
    }

    /// Elapsed age of the oldest active notice card, if any.
    pub fn oldest_notice_age(&self) -> Option<Duration> {
        self.app.notices.oldest_age(Instant::now())
    }
}

/// The `TERM=dumb` plain frontend in-process: `input` is what the user types (lines), the
/// result is everything printed. Ends when the input is used up and the host is idle.
pub async fn plain_session(opts: HostOptions, width: u16, input: &str) -> Result<String> {
    let ui = UiShared::new();
    let (tx, rx) = mpsc::unbounded_channel();
    *ui.tx.write().unwrap() = Some(tx);
    ui.width.store(width, std::sync::atomic::Ordering::Relaxed);
    let host = Host::with_ui(opts, ui.clone()).await?;
    let mut out = Vec::new();
    crate::plain::drive(host.clone(), ui, rx, input.as_bytes(), &mut out, None, false).await;
    host.shutdown().await;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

impl Vt {
    /// A mouse event at a screen cell (0-based column, row).
    pub fn mouse(&mut self, kind: mare_tui::tty::MouseEventKind, column: u16, row: u16) {
        let ev = mare_tui::tty::MouseEvent { kind, column, row, modifiers: Modifiers::NONE };
        handle_event(&mut self.app, &self.ui, Event::Mouse(ev));
    }

    /// Drain queued UI commands and app messages without drawing a frame.
    pub fn drain_cmds(&mut self) {
        self.drain();
    }

    /// Invoke post-handoff terminal reacquisition (used by handoff regression tests).
    pub fn reacquire_terminal(&mut self) {
        self.app.reacquire_terminal(app_now());
    }

    /// Queue an `AppMsg::SetEditor` (same path used by non-typing sources like Ctrl+R history).
    pub fn send_set_editor_msg(&mut self, text: &str) {
        let _ = self.app.app_tx.send(AppMsg::SetEditor(text.to_string()));
    }

    /// Current active image-hold deadline (if any committable front block is waiting on an image).
    pub fn image_hold_deadline(&self) -> Option<Instant> {
        self.app.image_hold_deadline()
    }

    /// Shared UI handle (for testing hooks like OSC 52 status propagation).
    pub fn ui(&self) -> &Arc<UiShared> {
        &self.ui
    }

    /// Whether select mode (mouse reporting) is on.
    pub fn selecting(&self) -> bool {
        self.app.select.active()
    }

    /// Run a keymap action as its key would.
    pub fn action(&mut self, a: &str) {
        self.app.run_action(a);
    }
}

/// Multiplier applied to wall-clock test wait budgets when Windows host startup, coverage
/// instrumentation, or high parallel nextest contention slows full-host startup.
fn test_timeout_scale_factor(is_windows: bool, coverage: bool, nextest_threads: Option<usize>, cores: usize) -> u32 {
    let high_parallel = nextest_threads.is_some_and(|n| n >= 8 && n >= cores);
    let mut factor = if is_windows { 4 } else { 1 };
    if coverage || high_parallel {
        factor *= 3;
    }
    factor
}

/// Scale a test wait budget on Windows or under coverage instrumentation (`LLVM_PROFILE_FILE` /
/// `MARE_LUA_COV_DIR`) or high parallel nextest contention (`NEXTEST_TEST_THREADS` at or above
/// CPU core count and >= 8), keeping idle Unix bounds tight while avoiding flakes on loaded runners.
pub fn scaled_test_timeout(base: Duration) -> Duration {
    let coverage = std::env::var_os("LLVM_PROFILE_FILE").is_some() || std::env::var_os("MARE_LUA_COV_DIR").is_some();
    let threads = std::env::var("NEXTEST_TEST_THREADS").ok().and_then(|v| v.parse::<usize>().ok());
    let cores = std::thread::available_parallelism().map_or(8, std::num::NonZeroUsize::get);
    base.saturating_mul(test_timeout_scale_factor(cfg!(windows), coverage, threads, cores))
}

impl Drop for Vt {
    fn drop(&mut self) {
        if let Some(startup) = self.startup.take() {
            startup.abort();
        }
        let _ = mare_tui::out::capture(false);
    }
}

#[cfg(test)]
mod tests {
    use super::test_timeout_scale_factor;

    #[test]
    fn timeout_scale_factor_triggers_on_windows_coverage_or_oversubscribed_parallelism() {
        assert_eq!(test_timeout_scale_factor(false, false, None, 14), 1);
        assert_eq!(test_timeout_scale_factor(false, false, Some(4), 14), 1);
        assert_eq!(test_timeout_scale_factor(false, false, Some(6), 6), 1);
        assert_eq!(test_timeout_scale_factor(false, true, None, 14), 3);
        assert_eq!(test_timeout_scale_factor(false, false, Some(24), 14), 3);
        assert_eq!(test_timeout_scale_factor(false, false, Some(8), 8), 3);
        assert_eq!(test_timeout_scale_factor(true, false, None, 16), 4);
        assert_eq!(test_timeout_scale_factor(true, true, None, 16), 12);
        assert_eq!(test_timeout_scale_factor(true, false, Some(16), 16), 12);
    }
}
