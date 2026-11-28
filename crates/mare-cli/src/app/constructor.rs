//! `App::new`.

use super::*;

impl App {
    /// The app state for a `w`×`h` terminal (no terminal is touched: `run` and the e2e
    /// [`testkit`] both start here).
    pub(super) fn new(
        host: Arc<Host>,
        ui: Arc<UiShared>,
        app_tx: mpsc::UnboundedSender<AppMsg>,
        w: u16,
        h: u16,
    ) -> Self {
        let keymap = ui.keymap.read().unwrap().clone();
        let startup_hint = crate::terminal::first_launch_hint(&mare_shared::paths::state_dir());
        let layout = ui.layout.read().unwrap().clone();
        let editor = SharedEditor::default();
        ui.install_editor_bridge(Arc::new(editor.clone()));
        let term_color = host.kernel.config().pointer("/terminal/color").and_then(|v| v.as_str()).map(str::to_string);
        let mut renderer = Renderer::new(w as usize);
        renderer.color =
            mare_shared::style::ColorMode::resolve(term_color.as_deref(), &|k| std::env::var(k).ok(), None);
        let mut app = Self {
            host,
            ui,
            blocks: vec![],
            committed: 0,
            widgets: BTreeMap::new(),
            status: BTreeMap::new(),
            status_seq: 0,
            status_known_seq: std::collections::HashMap::new(),
            working: None,
            // Windows: one-time hint about the console host / Windows Terminal keys
            notices: Notices::new(startup_hint.clone()),
            editor,
            last_editor_revision: 0,
            last_editor_text: String::new(),
            startup: StartupGate::new(),
            hint: None,
            modal: None,
            modal_stack: vec![],
            last_editor_activity: None,
            queue_edit_loading: false,
            delayed_approvals: Default::default(),
            pending_prefix: None,
            completion: CompletionState::default(),
            keymap,
            history: PromptHistory::load(),
            renderer,
            frame: 0,
            width: w as usize,
            height: h as usize,
            quit_armed: None,
            interrupted: None,
            in_tmux: std::env::var_os("TMUX").is_some(),
            quit: false,
            exit_guard: false,
            suspend_requested: false,
            external_editor_requested: false,
            expand_all: false,
            show_reasoning: false,
            external_editor: None,
            deferred_clear: false,
            clear_count: 0,
            deferred_title: None,
            cpr_pending: false,
            cpr_stale_replies: 0,
            cpr_cursor_row: 0,
            cpr_position_epoch: 0,
            transcript: TranscriptView::default(),
            editor_prompt: "❯ ".into(),
            editor_placeholder: None,
            editor_busy_placeholder: None,
            shell_style: None,
            editor_border: None,
            attachments: vec![],
            image_counter: 0,
            app_tx,
            reflow: Default::default(),
            burst: Default::default(),
            burst_enabled: cfg!(windows),
            conflict_table: mare_tui::keyconflict::builtin(),
            paste_guard: Default::default(),
            reflow_cap: mare_tui::reflow::max_rows(&|k| std::env::var(k).ok()),
            held: Default::default(),
            held_seq: 0,
            image_hold: Default::default(),
            margin: 0,
            theme_cache: Default::default(),
            reduce_motion: false,
            stream_commit_pending: false,
            component_caches: Default::default(),
            select: SelectMode::new(),
            layout,
            probe_state: Default::default(),
            probe_complete: false,
            late_reprobed: false,
        };
        app.renderer.height = h as usize;
        if let Some(text) = startup_hint {
            app.ui.record_notification(&text, "warning");
        }
        app
    }
}
