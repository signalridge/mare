//! The input box while missing packages install.

use crate::app::*;

/// Input available while a missing package is being installed. Nothing is sent or kept for later
/// until the full host (including configured plugins and permissions) has started: Enter leaves
/// the text in the input box and says so.
pub(in crate::app) struct StartupInput {
    pub(in crate::app) editor: Editor,
    pub(in crate::app) renderer: Renderer,
    /// Enter was pressed: the status line says startup is still in progress
    refused: bool,
    pub(in crate::app) width: u16,
    pub(in crate::app) height: u16,
    margin: usize,
    /// the host's latest installation progress line
    pub(in crate::app) progress: Option<String>,
}

impl StartupInput {
    pub(in crate::app) fn new(width: u16, height: u16, margin: usize) -> Self {
        let mut renderer = Renderer::new((width as usize).saturating_sub(margin).max(1));
        renderer.height = height as usize;
        Self { editor: Editor::new(), renderer, refused: false, width, height, margin, progress: None }
    }

    pub(in crate::app) fn render(&mut self, ui: &UiShared) {
        let theme = ui.theme.read().unwrap().clone();
        let opts = EditorRenderOpts { placeholder: "Type while plugins install".into(), ..Default::default() };
        let edit = self.editor.render(self.renderer.width, &theme, &opts);
        let what = self.progress.as_deref().unwrap_or("Installing required plugins…");
        let status = if self.refused {
            format!("{what} — startup is still in progress; press Enter once it's ready")
        } else {
            what.to_string()
        };
        let mut lines = vec![Line::raw(status)];
        lines.extend(edit.lines);
        let cursor = Some((edit.cursor.0 as usize + 1, edit.cursor.1 as usize));
        self.renderer.draw(&[], &lines, cursor);
    }

    /// Returns false when the user cancels startup.
    pub(in crate::app) fn handle(&mut self, event: Event, ui: &UiShared) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        match event {
            Event::Key(key) => {
                if key.kind == KeyEventKind::Press && key.modifiers.contains(Modifiers::CONTROL) {
                    if key.code == KeyCode::Char('c') {
                        if self.editor.text().is_empty() {
                            return false;
                        }
                        self.editor.clear_undoable();
                        return true;
                    }
                    if key.code == KeyCode::Char('d') && self.editor.text().is_empty() {
                        return false;
                    }
                }
                if let EditorAction::Submit(text) = self.editor.handle_key(key) {
                    self.editor.set_text(&text);
                    self.refused |= !text.trim().is_empty();
                }
            }
            Event::Paste(text) => self.editor.handle_paste(&text),
            Event::FocusIn => {
                ui.focused.store(true, Relaxed);
                ui.focus_known.store(true, Relaxed);
            }
            Event::FocusOut => {
                ui.focused.store(false, Relaxed);
                ui.focus_known.store(true, Relaxed);
            }
            Event::WindowResized(size) => {
                self.width = size.cols.max(1);
                self.height = size.rows.max(1);
                let lw = (self.width as usize).saturating_sub(self.margin).max(1);
                ui.width.store(lw as u16, Relaxed);
                ui.height.store(self.height, Relaxed);
                if let (Some(pw), Some(ph)) = (size.pixel_width, size.pixel_height) {
                    let (cw, ch) = (pw as u32 / self.width as u32, ph as u32 / self.height as u32);
                    if cw > 0 && ch > 0 {
                        mare_tui::image::set_cell_px(cw, ch);
                    }
                }
                self.renderer.resize(lw, self.height as usize);
            }
            _ => {}
        }
        true
    }
}
