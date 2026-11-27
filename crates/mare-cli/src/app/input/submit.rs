//! Submitting the input: Enter, steer and follow-up.

use crate::app::*;

/// Input that is no message for the model: a slash command (the commands plugin's test: a
/// leading `/` whose first word has no further `/`, so `/usr/bin/env …` stays a prompt) or `!cmd`.
fn is_command_or_shell(text: &str) -> bool {
    text.starts_with('!')
        || text.strip_prefix('/').is_some_and(|rest| !rest.split_whitespace().next().unwrap_or("").contains('/'))
}

impl App {
    pub(super) fn submit(&mut self, text: String) {
        if text.trim().is_empty() || self.refused_during_startup(&text) {
            return;
        }
        self.record_editor_activity(app_now());
        self.history.record(&text);
        self.history.reset();
        // a completion request still on its way is for the text just sent: it must not reopen
        // the menu over the empty input
        self.completion.invalidate();
        let elements = self.ed().take_submitted_elements();
        let (images, image_markers) = self.take_images(&elements);
        self.dispatch(text, images, image_markers);
        self.sync_editor_state();
    }

    /// Ctrl+S / Alt+Enter: `event` (`input/steer` or `input/follow-up`) with the input and its
    /// images; a slash command or a `!cmd` goes through [`Self::submit`] like Enter instead.
    pub(super) fn send_special(&mut self, event: &'static str) {
        let (text, elements) = self.ed().text_with_elements();
        self.ed().clear();
        if is_command_or_shell(&text) {
            self.ed().set_submitted_elements(elements);
            return self.submit(text);
        }
        if text.trim().is_empty() || self.refused_during_startup(&text) {
            self.sync_editor_state();
            return;
        }
        self.record_editor_activity(app_now());
        if self.ui.queue_mode.load(std::sync::atomic::Ordering::Relaxed) == mare_tui::queue_keys::EDITING {
            self.cancel_queue_mode(true);
        }
        self.history.record(&text);
        self.history.reset();
        self.completion.invalidate();
        let (images, image_markers) = self.take_images(&elements);
        self.emit(event, serde_json::json!({ "text": text, "images": images, "image_markers":image_markers }));
        self.sync_editor_state();
    }
}
