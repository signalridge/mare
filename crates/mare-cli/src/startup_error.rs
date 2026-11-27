//! The startup config error dialog shown before the main UI when init.lua (or the trusted project's
//! `.mare/init.lua`) failed: the file and line, the error's first line, and three choices —
//! open the file in the editor at that line (then run the config again and ask again while it
//! still fails), exit, or continue (the UI starts; every model request stays blocked until the
//! file is fixed and /reload succeeds). Headless and RPC have no dialog: they exit with 2.

use std::sync::Arc;

use anyhow::Result;
use mare_host::{ConfigError, Host};
use mare_shared::style::{Line, Span, Theme, display_path, wrap_line};
use mare_shared::ui::UiShared;
use mare_tui::fallback::{self, Fallback};
use mare_tui::term::Renderer;
use mare_tui::tty::{self, Event, KeyCode, KeyEvent, KeyEventKind, Modifiers};

/// What the user chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    Edit,
    Exit,
    Continue,
}

const CHOICES: [Choice; 3] = [Choice::Edit, Choice::Exit, Choice::Continue];

/// The dialog's state: the failed files, the highlighted choice, and the editor it would open.
pub(crate) struct Dialog {
    errors: Vec<ConfigError>,
    selected: usize,
    editor: String,
    /// the editor could not be started (shown above the choices)
    note: Option<String>,
}

impl Dialog {
    pub(crate) fn new(errors: Vec<ConfigError>, editor: String) -> Self {
        Self { errors, selected: 0, editor, note: None }
    }

    /// The highlighted choice on Enter, a digit's choice, Exit on Esc / Ctrl+C / Ctrl+D.
    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<Choice> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(Modifiers::CONTROL);
        match key.code {
            KeyCode::Char('c' | 'd') if ctrl => Some(Choice::Exit),
            KeyCode::Escape => Some(Choice::Exit),
            KeyCode::Enter => Some(CHOICES[self.selected]),
            KeyCode::Char(d @ '1'..='3') => Some(CHOICES[d as usize - '1' as usize]),
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                self.selected = (self.selected + CHOICES.len() - 1) % CHOICES.len();
                None
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                self.selected = (self.selected + 1) % CHOICES.len();
                None
            }
            _ => None,
        }
    }

    /// The dialog's rows for a `width`-column live area.
    pub(crate) fn lines(&self, width: usize, theme: &Theme) -> Vec<Line> {
        let w = width.clamp(24, 80);
        let inner = w - 4;
        let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
        let user = mare_config::user_init();
        let mut rows = vec![];
        for e in &self.errors {
            let name = if e.path == user { "init.lua" } else { ".mare/init.lua" };
            let place = display_path(&e.path.to_string_lossy(), &home, inner.saturating_sub(4));
            let place = match e.line {
                Some(l) => format!("{place}:{l}"),
                None => place,
            };
            rows.push(Line::new(vec![Span::styled(format!("{name} has an error"), theme.fg("MareError").bold())]));
            rows.push(Line::new(vec![Span::styled(place, theme.fg("MareMuted"))]));
            rows.extend(wrap_line(&Line::new(vec![Span::styled(e.message.clone(), theme.fg("MareError"))]), inner));
            rows.push(Line::empty());
        }
        let explain = "The part before the error ran; the rest of the file did not. Until it is fixed and \
                       /reload succeeds, model requests are blocked.";
        rows.extend(wrap_line(&Line::new(vec![Span::styled(explain, theme.fg("MareMuted"))]), inner));
        rows.push(Line::empty());
        if let Some(n) = &self.note {
            rows.extend(wrap_line(&Line::new(vec![Span::styled(n.clone(), theme.fg("MareWarning"))]), inner));
            rows.push(Line::empty());
        }
        let labels = [
            format!("Open in editor ({})", self.editor.split_whitespace().next().unwrap_or("vi")),
            "Exit and fix it yourself".to_string(),
            "Continue — requests stay blocked until /reload".to_string(),
        ];
        for (i, label) in labels.iter().enumerate() {
            let on = i == self.selected;
            let text = format!("{} {}. {label}", if on { "❯" } else { " " }, i + 1);
            let style = if on { theme.fg("MareAccent").bold() } else { theme.style("MareText") };
            rows.push(Line::new(vec![Span::styled(text, style)]));
        }
        rows.push(Line::empty());
        rows.push(Line::new(vec![Span::styled("↑/↓ choose · Enter confirm · Esc exit", theme.fg("MareDim"))]));
        let fb = Fallback::frame(
            vec![Span::styled("Config error", theme.style("MareTitle"))],
            "MareError",
            vec![Fallback::lines(rows)],
        );
        fallback::render(&fb, w, theme)
    }
}

/// Before the main UI: the dialog when a config file failed (true: start the UI, false: exit).
/// Uses the install screen's renderer and input when there was one, else its own (the input
/// reader is dropped again, so the startup probe still runs after the first frame).
pub(crate) async fn before_ui(
    host: &Arc<Host>,
    ui: &UiShared,
    boot_renderer: Option<&mut Renderer>,
    boot_events: Option<&mut tty::Events>,
) -> Result<bool> {
    if host.config_errors().is_empty() {
        return Ok(true);
    }
    let mut own_renderer = None;
    let mut own_events = None;
    let renderer = match boot_renderer {
        Some(r) => r,
        None => {
            let mut r = Renderer::new(ui.width.load(std::sync::atomic::Ordering::Relaxed) as usize);
            r.height = ui.height.load(std::sync::atomic::Ordering::Relaxed) as usize;
            own_renderer.insert(r)
        }
    };
    let events = match boot_events {
        Some(e) => e,
        None => own_events.insert(tty::events()?),
    };
    ask(host, ui, renderer, events).await
}

/// Show the dialog while a config file fails: true to start the UI, false to exit. Drawn with
/// `renderer` and erased when done; `events` is the terminal's input.
pub(crate) async fn ask(
    host: &Arc<Host>,
    ui: &UiShared,
    renderer: &mut Renderer,
    events: &mut tty::Events,
) -> Result<bool> {
    let errors = host.config_errors();
    if errors.is_empty() {
        return Ok(true);
    }
    let mut dialog = Dialog::new(errors, crate::editor_cmd::program(host));
    loop {
        let theme = ui.theme.read().unwrap().clone();
        renderer.draw(&[], &dialog.lines(renderer.width, &theme), None);
        let event = match events.next().await {
            Some(Ok(e)) => e,
            Some(Err(_)) | None => {
                renderer.erase();
                return Ok(false);
            }
        };
        let key = match event.event {
            Event::Key(k) => k,
            Event::FocusIn => {
                ui.focused.store(true, std::sync::atomic::Ordering::Relaxed);
                ui.focus_known.store(true, std::sync::atomic::Ordering::Relaxed);
                continue;
            }
            Event::FocusOut => {
                ui.focused.store(false, std::sync::atomic::Ordering::Relaxed);
                ui.focus_known.store(true, std::sync::atomic::Ordering::Relaxed);
                continue;
            }
            Event::WindowResized(size) => {
                let cols = size.cols.max(1);
                let rows = size.rows.max(1);
                ui.width.store(cols, std::sync::atomic::Ordering::Relaxed);
                ui.height.store(rows, std::sync::atomic::Ordering::Relaxed);
                if let (Some(pw), Some(ph)) = (size.pixel_width, size.pixel_height) {
                    let (cw, ch) = (pw as u32 / cols as u32, ph as u32 / rows as u32);
                    if cw > 0 && ch > 0 {
                        mare_tui::image::set_cell_px(cw, ch);
                    }
                }
                renderer.resize(cols as usize, rows as usize);
                continue;
            }
            _ => continue,
        };
        match dialog.key(key) {
            None => {}
            Some(Choice::Exit) => {
                renderer.erase();
                return Ok(false);
            }
            Some(Choice::Continue) => {
                renderer.erase();
                return Ok(true);
            }
            Some(Choice::Edit) => {
                let first = dialog.errors[0].clone();
                let program = dialog.editor.clone();
                renderer.erase();
                crate::app::hand_off_terminal();
                let status = crate::editor_cmd::command(&program, &first.path, first.line).map(|mut c| c.status());
                crate::app::take_back_terminal();
                if let Ok(ws) = tty::window_size() {
                    let cols = ws.cols.max(1);
                    let rows = ws.rows.max(1);
                    ui.width.store(cols, std::sync::atomic::Ordering::Relaxed);
                    ui.height.store(rows, std::sync::atomic::Ordering::Relaxed);
                    if let (Some(pw), Some(ph)) = (ws.pixel_width, ws.pixel_height) {
                        let (cw, ch) = (pw as u32 / cols as u32, ph as u32 / rows as u32);
                        if cw > 0 && ch > 0 {
                            mare_tui::image::set_cell_px(cw, ch);
                        }
                    }
                    renderer.resize(cols as usize, rows as usize);
                }
                renderer.invalidate();
                let started = matches!(status, Some(Ok(_)));
                // whatever the editor did, the file is read again
                let errors = host.retry_config().await;
                if errors.is_empty() {
                    return Ok(true);
                }
                dialog = Dialog::new(errors, program.clone());
                if !started {
                    dialog.note = Some(format!("Could not start the editor `{program}` (set $VISUAL or $EDITOR)"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog() -> Dialog {
        let path = mare_config::user_init();
        Dialog::new(vec![ConfigError { path, line: Some(3), message: "boom".into() }], "nvim".into())
    }

    fn text(d: &Dialog) -> String {
        d.lines(80, &Theme::default())
            .iter()
            .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn it_names_the_file_line_and_error_and_the_three_choices() {
        let t = text(&dialog());
        assert!(t.contains("Config error") && t.contains("init.lua has an error"), "{t}");
        assert!(t.contains("init.lua:3") && t.contains("boom"), "{t}");
        assert!(t.contains("❯ 1. Open in editor (nvim)") && t.contains("2. Exit") && t.contains("3. Continue"), "{t}");
    }

    #[test]
    fn keys_choose() {
        let k = |c| KeyEvent::new(c, Modifiers::NONE);
        let mut d = dialog();
        assert_eq!(d.key(k(KeyCode::Enter)), Some(Choice::Edit));
        assert_eq!(d.key(k(KeyCode::Down)), None);
        assert_eq!(d.key(k(KeyCode::Down)), None);
        assert_eq!(d.key(k(KeyCode::Enter)), Some(Choice::Continue));
        assert_eq!(d.key(k(KeyCode::Up)), None);
        assert_eq!(d.key(k(KeyCode::Enter)), Some(Choice::Exit));
        assert_eq!(d.key(k(KeyCode::Char('3'))), Some(Choice::Continue));
        assert_eq!(d.key(k(KeyCode::Escape)), Some(Choice::Exit));
        assert_eq!(d.key(KeyEvent::new(KeyCode::Char('c'), Modifiers::CONTROL)), Some(Choice::Exit));
    }
}
