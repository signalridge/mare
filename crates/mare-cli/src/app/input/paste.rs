//! Paste and image attachments.

use crate::app::*;

enum ClipboardPaste {
    Image(mare_binding::image::Encoded),
    Text(String),
}

fn choose_clipboard_paste(
    image: impl FnOnce() -> anyhow::Result<Option<mare_binding::image::Encoded>>,
    text: impl FnOnce() -> anyhow::Result<Option<String>>,
) -> anyhow::Result<Option<ClipboardPaste>> {
    match image()? {
        Some(image) => Ok(Some(ClipboardPaste::Image(image))),
        None => Ok(text()?.map(ClipboardPaste::Text)),
    }
}

impl App {
    pub(super) fn paste_clipboard(&mut self) -> bool {
        match choose_clipboard_paste(|| mare_binding::image::from_clipboard(2000), mare_binding::clipboard::read_text) {
            Ok(Some(ClipboardPaste::Image(img))) => self.attach(img),
            Ok(Some(ClipboardPaste::Text(text))) => self.handle_paste(&text),
            Ok(None) => return false,
            Err(e) => self.push_notice((format!("clipboard: {e}"), "warning".into(), Instant::now())),
        }
        true
    }
    /// Images live in the input as an atomic `[Image #N]` marker; deleting the marker drops the
    /// attachment. The marker stays in the text so the model can refer to it.
    fn fresh_image_marker(&mut self, reserved: &str) -> String {
        loop {
            self.image_counter += 1;
            let marker = format!("[Image #{}]", self.image_counter);
            if !reserved.contains(&marker)
                && !self.ed().text().contains(&marker)
                && !self.attachments.iter().any(|(_, existing, _)| existing == &marker)
            {
                return marker;
            }
        }
    }

    pub(super) fn attach(&mut self, img: mare_binding::image::Encoded) {
        let token = self.fresh_image_marker("");
        let block = serde_json::json!({ "type": "image", "data": img.data, "mime": img.mime });
        let id = self.ed().insert_token(&token, &token);
        self.attachments.push((id, token, block));
        self.sync_editor_state();
    }

    /// Attachments whose tracked editor element survived submission (deleted elements are dropped).
    /// Retains attachments still reachable via the editor's undo history or kill ring so post-submit
    /// undo and repeated/cross-submit yank keep their full image data.
    pub(super) fn take_images(
        &mut self,
        elements: &[mare_tui::editor::TextElementSnapshot],
    ) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
        let mut selected: Vec<_> = elements
            .iter()
            .filter_map(|elem| {
                self.attachments
                    .iter()
                    .find(|(id, _, _)| *id == elem.id)
                    .map(|(_, token, image)| (elem.range.start, token.clone(), image.clone()))
            })
            .collect();
        selected.sort_by_key(|(position, _, _)| *position);
        let live = self.ed().referenced_element_ids();
        self.attachments.retain(|(id, _, _)| live.contains(id));
        let mut images = Vec::new();
        let mut markers = Vec::new();
        for (position, token, image) in selected {
            images.push(image);
            markers.push(serde_json::json!({"text":token,"offset":position}));
        }
        (images, markers)
    }

    /// Reclaim image ownership on a rejected submission, returning the restored text and the
    /// exact byte range of each restored marker so registration never binds to lookalike prose.
    pub(in crate::app) fn restore_images(
        &mut self,
        text: &str,
        images: Vec<serde_json::Value>,
        markers: Vec<serde_json::Value>,
    ) -> (String, Vec<(std::ops::Range<usize>, String, serde_json::Value)>) {
        let (editor_text, active_elements) = self.ed().text_with_elements();
        let mut restored = String::new();
        let mut ranges = Vec::new();
        let mut rest = text;
        let mut consumed = 0;
        for (index, image) in images.into_iter().enumerate() {
            // Attachment identity comes from submission, never from a lookalike in prose.
            let found = markers.get(index).and_then(|identity| {
                let marker = identity.get("text")?.as_str()?;
                let offset = usize::try_from(identity.get("offset")?.as_u64()?).ok()?;
                if offset < consumed || text.get(offset..offset + marker.len())? != marker {
                    return None;
                }
                Some((offset - consumed, offset - consumed + marker.len(), marker.to_owned()))
            });
            let marker = if let Some((start, end, marker)) = found {
                restored.push_str(&rest[..start]);
                rest = &rest[end..];
                consumed += end;
                marker
            } else {
                restored.push_str(rest);
                consumed = text.len();
                rest = "";
                restored.push(' ');
                self.fresh_image_marker(text)
            };
            // Only collide against markers active in the current editor buffer, not attachments
            // retained in undo/kill history for post-submit undo or yank.
            let has_active_marker = self
                .attachments
                .iter()
                .any(|(id, token, _)| token == &marker && active_elements.iter().any(|elem| elem.id == *id));
            let marker =
                if has_active_marker || editor_text.contains(&marker) { self.fresh_image_marker(text) } else { marker };
            let start = restored.len();
            restored.push_str(&marker);
            let end = restored.len();
            ranges.push((start..end, marker, image));
        }
        restored.push_str(rest);
        (restored, ranges)
    }

    pub(in crate::app) fn handle_paste(&mut self, text: &str) {
        self.handle_paste_at(text, app_now());
    }

    pub(in crate::app) fn handle_paste_at(&mut self, text: &str, at: Instant) {
        self.paste_guard.pasted(at);
        if self.modal.is_none() {
            self.record_editor_activity(at);
        }
        match self.modal.as_mut() {
            Some(Modal::Input { editor, .. }) => editor.paste_inline(text.trim_end_matches(['\n', '\r'])),
            Some(Modal::Select { kind, allow_search, .. }) if kind == "confirm" || !*allow_search => {}
            Some(Modal::Select { filter, selected, tab, searching, .. }) => {
                let (prev_selected, prev_tab) = (*selected, *tab);
                let first_line = text.lines().next().unwrap_or("");
                if !first_line.is_empty() {
                    filter.push_str(first_line);
                    *searching = true;
                    *selected = 0;
                }
                self.finish_select_key(prev_selected, prev_tab, false);
                self.notify_highlight();
            }
            Some(Modal::Help { filter, selected, .. }) => {
                let first_line = text.lines().next().unwrap_or("");
                if !first_line.is_empty() {
                    filter.push_str(first_line);
                    *selected = 0;
                }
            }
            Some(Modal::Info { .. }) => {}
            None => {
                // Cmd+V of a copied image: many terminals deliver an *empty* bracketed paste —
                // fall back to reading the system clipboard for an image
                if text.is_empty() {
                    if let Ok(Some(img)) = mare_binding::image::from_clipboard(2000) {
                        self.attach(img);
                    }
                    return;
                }
                // terminals paste dropped files as (quoted / backslash-escaped / file://) paths,
                // several at once separated by spaces: when every one is an image file, each
                // becomes an `[Image #N]` attachment; anything else is pasted text
                let paths = mare_tui::paste::dropped_paths(text);
                let images = !paths.is_empty()
                    && paths.iter().all(|p| {
                        let p = std::path::Path::new(p);
                        mare_binding::image::is_image_path(p) && p.is_file()
                    });
                if images {
                    let mut loaded = vec![];
                    for p in &paths {
                        match mare_binding::image::load_path(std::path::Path::new(p), 2000) {
                            Ok(img) => loaded.push(img),
                            Err(e) => {
                                self.push_notice((format!("image: {e}"), "warning".into(), Instant::now()));
                                self.ed().handle_paste(text);
                                self.sync_editor_state();
                                return;
                            }
                        }
                    }
                    for (i, img) in loaded.into_iter().enumerate() {
                        if i > 0 {
                            self.ed().insert_str(" ");
                        }
                        self.attach(img);
                    }
                    return;
                }
                let inline = self.ed().handle_paste(text);
                self.sync_editor_state();
                if inline {
                    self.history.reset();
                    self.request_completion(false);
                } else {
                    self.completion.invalidate();
                }
            }
        }
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::*;

    #[test]
    fn missing_image_falls_back_to_text_and_errors_remain_errors() {
        let result = choose_clipboard_paste(|| Ok(None), || Ok(Some("pasted text".into()))).unwrap();
        assert!(matches!(result, Some(ClipboardPaste::Text(ref text)) if text == "pasted text"));
        assert!(choose_clipboard_paste(|| Err(anyhow::anyhow!("clipboard failure")), || Ok(None)).is_err());
    }
}
