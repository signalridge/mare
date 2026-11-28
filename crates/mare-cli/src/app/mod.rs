//! Interactive TUI frontend. Owns the terminal, the transcript blocks, widgets, status bar,
//! editor and modals; plugins drive it through `mare.ui` commands.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use mare_tui::tty::{self, Event, KeyCode, KeyEvent, KeyEventKind, Modifiers};
use tokio::sync::{mpsc, oneshot};

use mare_host::{Host, HostOptions};
use mare_shared::bindings::Keymap;
use mare_shared::style::*;
use mare_shared::ui::{BlockPatch, Dismissed, Node, SelectItem, SelectReply, SelectTab, UiCmd, UiShared};
use mare_tui::editor::{Editor, EditorAction, EditorRenderOpts};
use mare_tui::frame;
use mare_tui::keys::key_to_string;
use mare_tui::markdown::StreamingMarkdown;
use mare_tui::render::{STATIC_FRAME, render_node};
use mare_tui::term::Renderer;
use mare_tui::term_input::{decode_events, is_dark};
#[cfg(unix)]
use mare_tui::term_input::{parse_osc11, strip_replies};

mod constructor;
mod editor_bridge;
mod event_loop;
mod input;
mod startup;
mod term;
/// In-process driver for the TUI e2e tests: the real [`App`]
/// with its host, fed key / paste / resize events, its frames captured from the write exit
/// ([`mare_tui::out::capture`]) instead of a terminal. The test feeds those bytes into a virtual
/// terminal (vt100) and asserts on the screen. Not a public API.
pub mod testkit;
mod view;

use editor_bridge::SharedEditor;
use event_loop::handle_event;
pub(crate) use event_loop::resume_hint;
pub use event_loop::run;
use input::completion::{CompletionItem, CompletionState};
use input::prompt_history::PromptHistory;
use mare_shared::ui::EditorBridge;
use startup::gate::StartupGate;
use startup::trust::settle_and_ask_trust;
pub(crate) use term::term_guard::{hand_off_terminal, reclaim_terminal_for_exit, take_back_terminal};
use view::block::Block;
use view::modal::Modal;
use view::notices::Notices;
use view::scrollback::HeldCommit;
use view::select_filter::SelectFilterCache;
pub(crate) use view::select_filter::{filter_items, skip_unselectable};
use view::selection::SelectMode;
use view::transcript::TranscriptView;

/// Two Ctrl+C presses within this window exit.
const QUIT_WINDOW: Duration = Duration::from_secs(2);
const QUIT_HINT: &str = "Press Ctrl+C again to exit";
const STOP_HINT: &str = "stopping… (Ctrl+C again to force)";
/// Wait this long after the last editor typing activity before popping an incoming approval modal
/// (Codex `PROMPT_TYPING_IDLE_DELAY`).
const PROMPT_TYPING_IDLE_DELAY: Duration = Duration::from_secs(1);

enum AppMsg {
    Completions(u64, Vec<CompletionItem>, Option<(u64, String)>),
    SetEditor(String),
    /// the `ui/caps` waterfall's result (plugins may add or override capabilities)
    Caps(serde_json::Value),
    /// a held commit came back from `ui/commit-before` (None: keep the original lines)
    CommitReady(u64, Option<Vec<String>>),
    /// Trust and startup session work completed: prompts may be sent.
    StartupReady,
    StartupFailed(String, StartupFailure),
    /// What blocked startup was fixed (a session was started or loaded, or a reload succeeded):
    /// the block lifts (nothing is sent on its own).
    StartupRecovered,
    /// Selection clipboard copy completed on a blocking worker thread.
    CopyResult {
        chars: usize,
        bytes: usize,
        result: std::result::Result<mare_binding::clipboard::Method, String>,
    },
}

/// Why startup was blocked, which decides what lifts the block: a `-c` / `--resume` that found
/// no session lifts with a new or loaded session (`/new`, `/resume`), a failed trust / project
/// configuration with a successful `/reload`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum KeyFocus {
    Editor,
    Transcript,
    Select,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupFailure {
    Trust,
    Resume,
}

pub(in crate::app) struct Widget {
    pub(super) node: Node,
    pub(super) has_anim: bool,
}

pub(in crate::app) struct StatusSegment {
    pub(super) node: Node,
    pub(super) keep: bool,
    pub(super) has_anim: bool,
}

struct App {
    /// commits held for `ui/commit-before`
    held: std::collections::VecDeque<HeldCommit>,
    held_seq: u64,
    host: Arc<Host>,
    ui: Arc<UiShared>,
    blocks: Vec<Block>,
    committed: usize,
    widgets: BTreeMap<(String, i64, String), Widget>,
    status: BTreeMap<(String, i64, u64, String), StatusSegment>,
    status_seq: u64,
    status_known_seq: std::collections::HashMap<String, u64>,
    working: Option<(String, Option<String>, Instant, bool)>,
    /// notification cards, auto-dismissed
    notices: Notices,
    editor: SharedEditor,
    last_editor_revision: u64,
    #[allow(dead_code)]
    last_editor_text: String,
    /// the startup gate (ready / blocked / recovered, the command-line prompt)
    startup: StartupGate,
    hint: Option<String>,
    modal: Option<Modal>,
    /// modals under the current one (`?` help opened from a list returns to it)
    modal_stack: Vec<Modal>,
    /// Last time the user typed, edited, submitted, or pasted into the main editor.
    last_editor_activity: Option<Instant>,
    /// True between emitting `input/queue-edit` and receiving the initial editor load from
    /// `ui/queue_edit.lua`, so that load is not treated as an external replacement and a rapid
    /// second Enter on the still-empty buffer cannot delete the queued message.
    queue_edit_loading: bool,
    /// Approval modals waiting for typing to go idle or an active modal to close.
    delayed_approvals: std::collections::VecDeque<Modal>,
    /// which-key: a prefix key of multi-key bindings was pressed (`ctrl+x` of `ctrl+x g`)
    pending_prefix: Option<(String, KeyFocus)>,
    completion: CompletionState,
    keymap: Keymap,
    history: PromptHistory,
    renderer: Renderer,
    frame: usize,
    width: usize,
    height: usize,
    quit_armed: Option<Instant>,
    /// Esc / Ctrl+C interrupted running work: until it has stopped (the working line is gone and
    /// no process tree is still being stopped) another Ctrl+C forces the stop
    interrupted: Option<Instant>,
    /// inside tmux
    in_tmux: bool,
    quit: bool,
    /// set by `ui.exit_guard(true)` (the jobs plugin while background jobs run): a user's exit
    /// asks through `app/exit-requested` instead of quitting
    exit_guard: bool,
    /// Ctrl+O: compact vs expanded tool output / thinking for everything still on screen
    expand_all: bool,
    /// `/show-reasoning`: render reasoning blocks expanded for this TUI instance
    show_reasoning: bool,
    /// Ctrl+Z: the loop hands the terminal back and stops the process (SIGTSTP)
    suspend_requested: bool,
    /// Ctrl+G: the loop pauses tty input, hands off the terminal, and spawns the external editor
    external_editor_requested: bool,
    /// Active async external editor child (`Ctrl+G`), if one is currently running
    external_editor: Option<input::external_editor::ExternalEditorSession>,
    /// Deferred screen clear requested while the terminal was handed off to another program
    deferred_clear: bool,
    /// Deferred OSC 2 window title requested while the terminal was handed off
    deferred_title: Option<String>,
    /// In-flight in-band Cursor Position Report (`CSI 6n`) used to re-anchor `renderer.top`
    cpr_pending: bool,
    /// Invalidated in-flight `CSI 6n` queries whose stale terminal replies have not arrived yet
    cpr_stale_replies: u8,
    /// `renderer.cursor_row()` when the in-flight CPR query was emitted
    cpr_cursor_row: usize,
    /// `renderer.position_epoch()` when the in-flight CPR query was emitted
    cpr_position_epoch: u64,
    /// the Ctrl+T transcript overlay
    transcript: TranscriptView,
    editor_prompt: String,
    editor_placeholder: Option<String>,
    editor_busy_placeholder: Option<String>,
    /// shell mode's look; set by the plugin that owns it, `None` = no shell mode
    shell_style: Option<mare_shared::ui::ShellStyle>,
    editor_border: Option<String>,
    /// image attachments for the next message: (editor element id, label, image block)
    attachments: Vec<(u64, String, serde_json::Value)>,
    image_counter: usize,
    app_tx: mpsc::UnboundedSender<AppMsg>,
    /// key-conflict table
    conflict_table: Vec<mare_tui::keyconflict::Entry>,
    /// pastes without bracketed-paste markers / the stray Enter after one
    burst: mare_tui::paste::PasteBurst,
    /// burst detection only where pastes may arrive unbracketed: on by default on Windows, whose
    /// legacy console has no bracketed paste
    burst_enabled: bool,
    paste_guard: mare_tui::paste::PasteGuard,
    /// resize reflow: debounce state, per-terminal row cap
    reflow: mare_tui::reflow::ReflowState,
    reflow_cap: usize,
    /// how long the oldest finished block has waited for its pictures
    image_hold: mare_tui::image_hold::Hold,
    /// cells kept free at the end of each row under tmux / screen: the layout width is the
    /// terminal width minus this
    margin: usize,
    theme_cache: std::cell::RefCell<Option<(u64, Arc<Theme>)>>,
    /// `mare.o.reduce_motion`: spinners draw a static marker, frames don't animate
    reduce_motion: bool,
    /// True when the active streaming assistant block still has queued stable rows to drain.
    stream_commit_pending: bool,
    component_caches: Box<view::lua_components::ComponentCaches>,
    /// select mode: mouse selection in the live area, copied on release
    select: SelectMode,
    /// validated stacked region layout (`ui.layout`)
    layout: mare_shared::layout::LayoutConfig,
    /// cumulative terminal capability replies (startup probe + late in-band replies)
    probe_state: mare_tui::term_input::Probe,
    /// whether the terminal has answered our probe fence (DA1) at least once
    probe_complete: bool,
    /// whether we already triggered a one-shot in-band re-probe after a startup timeout
    late_reprobed: bool,
}

impl App {
    pub(in crate::app) fn ed(&self) -> std::sync::MutexGuard<'_, Editor> {
        self.editor.lock()
    }

    pub(in crate::app) fn sync_editor_state(&mut self) {
        let Some(synced) = self.editor.sync_state(&self.ui.editor_snapshot, self.last_editor_revision) else {
            let shell_level = self.ed().shell_level();
            self.completion.set_shell_level(shell_level);
            return;
        };
        self.completion.set_shell_level(synced.shell_level);
        if let Some(live) = synced.live_elements {
            self.attachments.retain(|(id, _, _)| live.contains(id));
            if synced.lua_text_changed {
                self.history.reset();
                self.request_completion(false);
                if self.queue_edit_loading {
                    self.queue_edit_loading = false;
                } else if self.ui.queue_mode.load(std::sync::atomic::Ordering::Relaxed) == mare_tui::queue_keys::EDITING
                {
                    self.cancel_queue_mode(true);
                }
            }
        }
        if synced.revision != self.last_editor_revision {
            self.last_editor_revision = synced.revision;
            if self.host.kernel.listener_count("editor/changed") > 0 {
                let sel = synced.selection.map(|((sr, sc), (er, ec))| {
                    let (ar, ac) = synced.anchor.unwrap_or((sr, sc));
                    let (hr, hc) = synced.head.unwrap_or((er, ec));
                    serde_json::json!({
                        "start": { "row": sr, "col": sc },
                        "end": { "row": er, "col": ec },
                        "anchor": { "row": ar, "col": ac },
                        "head": { "row": hr, "col": hc },
                        "text": synced.selected_text,
                    })
                });
                self.emit(
                    "editor/changed",
                    serde_json::json!({
                        "text": synced.text.as_str(),
                        "cursor": { "row": synced.cursor.0, "col": synced.cursor.1 },
                        "selection": sel,
                        "revision": synced.revision,
                    }),
                );
            }
        }
    }

    /// The shared theme, cloned once per change instead of once per frame (its ~100 named styles
    /// were a per-frame allocation cost): reused while [`mare_shared::style::theme_gen`] holds.
    fn theme(&self) -> Arc<Theme> {
        let generation = mare_shared::style::theme_gen();
        if let Some((g, t)) = self.theme_cache.borrow().as_ref()
            && *g == generation
        {
            return t.clone();
        }
        let mut t = self.ui.theme.read().unwrap().clone();
        if self.renderer.color == ColorMode::NoColor {
            // NO_COLOR: a selected row is reverse video, not a background
            let sel = t.style("MareSelection");
            t.set_style("MareSelection", sel.bold().reverse());
        }
        let t = Arc::new(t);
        *self.theme_cache.borrow_mut() = Some((generation, t.clone()));
        t
    }

    /// The animation frame to draw ([`STATIC_FRAME`] under `reduce_motion`, otherwise
    /// [`mare_tui::render::WALL_CLOCK_FRAME`] so `Node::Spinner` advances by wall clock).
    fn anim_frame(&self) -> usize {
        if self.reduce_motion { STATIC_FRAME } else { mare_tui::render::WALL_CLOCK_FRAME }
    }

    // --------------------------------------------------------------
    // input
    // --------------------------------------------------------------
    fn emit(&self, event: &'static str, payload: serde_json::Value) {
        let h = self.host.clone();
        tokio::task::spawn_local(async move { h.emit_json(event, payload).await });
    }

    fn push_notice(&mut self, notice: (String, String, Instant)) {
        self.push_notice_with_id(notice, None);
    }

    fn push_notice_with_id(&mut self, notice: (String, String, Instant), id: Option<String>) {
        self.ui.record_notification(&notice.0, &notice.1);
        self.notices.push_with_id(notice, id);
    }

    fn push_notice_for_id(&mut self, text: &str, level: &str, life: Duration, id: &str) {
        self.notices.push_for_with_id(text, level, life, id);
        self.ui.record_notification(text, level);
    }

    pub(in crate::app) fn disarm_quit(&mut self) {
        self.quit_armed = None;
        self.notices.remove_by_id(QUIT_NOTICE_ID);
    }

    pub(in crate::app) fn reset_after_editor_replace(&mut self) {
        self.sync_editor_state();
        self.history.reset();
        self.request_completion(false);
    }

    pub(in crate::app) fn sync_position_known(&self) {
        let known = self.renderer.scroll_insert_ready();
        let mut caps = self.ui.caps.write().unwrap();
        if let Some(obj) = caps.as_object_mut() {
            obj.insert("position_known".into(), serde_json::Value::Bool(known));
        }
    }

    pub(in crate::app) fn invalidate_cpr(&mut self) {
        if std::mem::take(&mut self.cpr_pending) {
            self.cpr_stale_replies = self.cpr_stale_replies.saturating_add(1);
        }
    }

    pub(in crate::app) fn maybe_request_cpr(&mut self) {
        if self.renderer.caps.scroll_region
            && self.renderer.top().is_none()
            && !self.cpr_pending
            && self.cpr_stale_replies == 0
        {
            self.cpr_pending = true;
            self.cpr_cursor_row = self.renderer.cursor_row();
            self.cpr_position_epoch = self.renderer.position_epoch();
            mare_tui::out::write_own(b"\x1b[6n".to_vec());
        }
    }

    pub(in crate::app) fn handle_cpr_reply(&mut self, row: usize) {
        if self.cpr_stale_replies > 0 {
            self.cpr_stale_replies -= 1;
            if self.cpr_stale_replies == 0 && !self.transcript.is_open() {
                self.maybe_request_cpr();
            }
            return;
        }
        let was_pending = self.cpr_pending;
        self.cpr_pending = false;
        if was_pending
            && self.renderer.top().is_none()
            && self.renderer.cursor_row() == self.cpr_cursor_row
            && self.renderer.position_epoch() == self.cpr_position_epoch
        {
            self.probe_state.cursor_row = Some(row);
            self.renderer.anchor(row);
            self.sync_position_known();
        } else if was_pending && !self.transcript.is_open() {
            // The reply belonged to an earlier frame whose live-area row may have shifted; re-query.
            self.maybe_request_cpr();
        }
    }

    /// Update the shared terminal cell pixel dimensions across `probe_state`, published `ui.caps`,
    /// and the image renderer so resize, handoff reacquire, and in-band probes share one source of truth.
    pub(in crate::app) fn update_cell_px(&mut self, cw: u32, ch: u32, now: Instant) -> bool {
        if cw == 0 || ch == 0 {
            return false;
        }
        self.probe_state.cell_px = Some((cw, ch));
        if let Some(obj) = self.ui.caps.write().unwrap().as_object_mut() {
            obj.insert("cell_px".into(), serde_json::json!([cw, ch]));
        }
        let before = mare_tui::image::generation();
        mare_tui::image::set_cell_px(cw, ch);
        let changed = mare_tui::image::generation() != before;
        if changed {
            self.transcript.invalidate_cache();
            self.reflow.invalidated(now);
        }
        changed
    }

    /// Single entry point for terminal dimension updates (resize event, post-handoff sync).
    pub(in crate::app) fn apply_terminal_size(&mut self, cols: u16, rows: u16, now: Instant) {
        let lw = self.layout_width(cols);
        self.width = lw as usize;
        self.height = rows as usize;
        self.ui.width.store(lw, std::sync::atomic::Ordering::Relaxed);
        self.ui.height.store(rows, std::sync::atomic::Ordering::Relaxed);
        if self.transcript.is_open() {
            self.renderer.width = cols as usize;
            self.renderer.height = (rows as usize).max(1);
            self.renderer.invalidate();
        } else {
            self.renderer.resize(cols as usize, rows as usize);
        }
        self.invalidate_cpr();
        self.sync_position_known();
        self.reflow.resized(lw as usize, now);
        self.emit("ui/resize", serde_json::json!({ "width": lw, "height": rows }));
    }

    /// Unified post-handoff terminal reacquisition (`SIGCONT` and external editor return).
    pub(in crate::app) fn reacquire_terminal(&mut self, now: Instant) {
        self.ui.focus_known.store(false, std::sync::atomic::Ordering::Relaxed);
        self.renderer.invalidate();
        self.cpr_pending = false;
        self.cpr_stale_replies = 0;
        self.sync_position_known();
        if let Ok(ws) = tty::window_size() {
            let (w, h) = (ws.cols, ws.rows);
            let lw = self.layout_width(w);
            if lw as usize != self.width
                || h as usize != self.height
                || w as usize != self.renderer.width
                || h as usize != self.renderer.height
            {
                self.apply_terminal_size(w, h, now);
            }
            if let (Some(pw), Some(ph)) = (ws.pixel_width, ws.pixel_height)
                && w > 0
                && h > 0
            {
                let (cw, ch) = (pw as u32 / w as u32, ph as u32 / h as u32);
                self.update_cell_px(cw, ch, now);
            }
        }
        if std::mem::take(&mut self.deferred_clear) && !self.transcript.is_open() {
            self.renderer.clear_screen();
        }
        if let Some(title) = self.deferred_title.take() {
            mare_tui::out::title(&title);
        }
        if self.transcript.is_open() {
            self.transcript.frame.clear();
        }
    }

    /// A user asked to leave (quit keys, `/exit`): with an exit guard set, its owner decides
    /// (`app/exit-requested` → `ui.exit{force=true}` once confirmed); signals and a closed
    /// terminal never ask.
    fn request_quit(&mut self) {
        if self.exit_guard {
            self.emit("app/exit-requested", serde_json::json!({}));
        } else {
            self.quit = true;
        }
    }

    /// Esc / Ctrl+C: tell the plugins (`key/interrupt`); interrupting running work starts the
    /// "stopping" phase in which another Ctrl+C forces the stop.
    fn interrupt(&mut self) {
        if self.working.is_some() {
            self.interrupted = Some(Instant::now());
        }
        // double-Esc opens the /undo picker only with an empty input
        self.emit("key/interrupt", serde_json::json!({ "editor_empty": self.ed().is_empty() }));
    }

    /// The interrupted work hasn't stopped yet: its working line is still
    /// up, or a process tree is still being stopped.
    fn stopping(&self) -> bool {
        self.interrupted.is_some() && (self.working.is_some() || mare_binding::stopping::active())
    }

    fn dispatch(&mut self, text: String, images: Vec<serde_json::Value>, image_markers: Vec<serde_json::Value>) {
        let h = self.host.clone();
        tokio::task::spawn_local(async move { h.submit_draft(&text, images, image_markers).await });
    }

    /// The width layout uses for a terminal `w` cells wide (the tmux / screen margin taken off).
    fn layout_width(&self, w: u16) -> u16 {
        w.saturating_sub(self.margin as u16).max(1)
    }

    pub(in crate::app) fn is_approval_modal(&self) -> bool {
        matches!(self.modal, Some(Modal::Select { approval: true, .. }))
    }

    pub(in crate::app) fn prompt_delay_remaining(&self, now: Instant) -> Option<Duration> {
        self.last_editor_activity.and_then(|last| {
            last.checked_add(PROMPT_TYPING_IDLE_DELAY)
                .and_then(|show_at| show_at.checked_duration_since(now))
                .filter(|delay| !delay.is_zero())
        })
    }

    pub(in crate::app) fn record_editor_activity(&mut self, now: Instant) {
        self.last_editor_activity = Some(now);
    }

    pub(in crate::app) fn delayed_prompt_deadline(&self, now: Instant) -> Option<Instant> {
        if self.delayed_approvals.is_empty() || self.modal.is_some() {
            return None;
        }
        Some(match self.prompt_delay_remaining(now) {
            Some(delay) => now + delay,
            None => now,
        })
    }

    pub(in crate::app) fn activate_select_modal(&mut self, modal: Modal) {
        self.disarm_quit();
        if self.select.active() {
            self.leave_select_mode();
        }
        self.push_overlay_under_modal();
        self.reset_select_row_failures();
        self.modal = Some(modal);
        self.notify_highlight();
    }

    pub(in crate::app) fn maybe_show_delayed_prompts(&mut self, now: Instant) -> bool {
        if self.delayed_approvals.is_empty() || self.modal.is_some() {
            return false;
        }
        if self.prompt_delay_remaining(now).is_some() {
            return false;
        }
        let Some(modal) = self.delayed_approvals.pop_front() else {
            return false;
        };
        self.activate_select_modal(modal);
        true
    }

    pub(in crate::app) fn mark_modal_drawn(&mut self) {
        if let Some(Modal::Select { drawn, .. }) = self.modal.as_mut() {
            *drawn = true;
        }
    }

    pub(in crate::app) fn pop_modal_or_promote_delayed(&mut self) {
        self.modal = None;
        if let Some(mut popped) = self.modal_stack.pop() {
            if let Modal::Select { approval: true, drawn, .. } = &mut popped {
                *drawn = false;
                if self.prompt_delay_remaining(app_now()).is_some() {
                    self.delayed_approvals.push_front(popped);
                    return;
                }
            }
            self.modal = Some(popped);
            self.notify_highlight();
        } else {
            self.maybe_show_delayed_prompts(app_now());
        }
    }

    /// Exit queue selection or editing mode and notify Lua (`input/queue-cancel`).
    /// Returns the previous queue mode.
    pub(in crate::app) fn cancel_queue_mode(&mut self, keep_editor: bool) -> u8 {
        self.queue_edit_loading = false;
        let mode = self.ui.queue_mode.swap(mare_tui::queue_keys::OFF, std::sync::atomic::Ordering::Relaxed);
        if mode != mare_tui::queue_keys::OFF {
            self.emit("input/queue-cancel", serde_json::json!({ "keep_editor": keep_editor }));
        }
        mode
    }

    /// Replace the editor text from a non-typing source (e.g. Ctrl+R history search),
    /// cancelling any active queue-message edit so submitting will not overwrite the queue.
    pub(in crate::app) fn apply_set_editor(&mut self, text: String) {
        if let Some(session) = self.external_editor.as_mut() {
            session.pending_editor_cmds.push(UiCmd::SetEditor {
                text,
                images: Vec::new(),
                image_markers: Vec::new(),
                keep_draft: false,
                prepend: false,
            });
            return;
        }
        if self.ui.queue_mode.load(std::sync::atomic::Ordering::Relaxed) == mare_tui::queue_keys::EDITING {
            self.cancel_queue_mode(true);
        }
        self.ed().set_text(&text);
        self.reset_after_editor_replace();
    }
}

pub(in crate::app) const QUIT_NOTICE_ID: &str = "quit-confirm";

/// Monotonic timestamp aligned with tokio's clock when running under a runtime (enabling paused-time tests).
pub(in crate::app) fn app_now() -> Instant {
    if tokio::runtime::Handle::try_current().is_ok() { tokio::time::Instant::now().into_std() } else { Instant::now() }
}

impl Drop for App {
    fn drop(&mut self) {
        self.ui.clear_editor_bridge();
    }
}
