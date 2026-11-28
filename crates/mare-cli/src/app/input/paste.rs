//! Paste and image attachments.

use crate::app::*;

pub(in crate::app) enum ClipboardPaste {
    Image(mare_binding::image::Encoded),
    Text(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum PasteTarget {
    MainEditor { clear_count: usize },
    ModalInput,
    ModalSelect,
    ModalHelp,
    Ignored,
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
    fn capture_paste_target(&self) -> PasteTarget {
        match &self.modal {
            None => PasteTarget::MainEditor { clear_count: self.clear_count },
            Some(Modal::Input { .. }) => PasteTarget::ModalInput,
            Some(Modal::Select { kind, allow_search, .. }) if kind != "confirm" && *allow_search => {
                PasteTarget::ModalSelect
            }
            Some(Modal::Help { .. }) => PasteTarget::ModalHelp,
            Some(_) => PasteTarget::Ignored,
        }
    }

    fn paste_target_matches(&self, target: PasteTarget) -> bool {
        match target {
            PasteTarget::MainEditor { clear_count } => self.modal.is_none() && self.clear_count == clear_count,
            PasteTarget::ModalInput => matches!(self.modal, Some(Modal::Input { .. })),
            PasteTarget::ModalSelect => {
                matches!(&self.modal, Some(Modal::Select { kind, allow_search, .. }) if kind != "confirm" && *allow_search)
            }
            PasteTarget::ModalHelp => matches!(self.modal, Some(Modal::Help { .. })),
            PasteTarget::Ignored => false,
        }
    }

    pub(super) fn paste_clipboard(&mut self) -> bool {
        let target = self.capture_paste_target();
        let app_tx = self.app_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = choose_clipboard_paste(
                || mare_binding::image::from_clipboard(2000),
                mare_binding::clipboard::read_text,
            )
            .map_err(|e| e.to_string());
            let _ = app_tx.send(AppMsg::ClipboardPasteReady { target, report_error: true, result });
        });
        true
    }

    pub(in crate::app) fn handle_clipboard_paste_result(
        &mut self,
        target: PasteTarget,
        report_error: bool,
        result: std::result::Result<Option<ClipboardPaste>, String>,
    ) {
        match result {
            Ok(Some(ClipboardPaste::Image(img))) => {
                if matches!(target, PasteTarget::MainEditor { .. }) && self.paste_target_matches(target) {
                    self.record_editor_activity(app_now());
                    self.attach(img);
                }
            }
            Ok(Some(ClipboardPaste::Text(text))) => {
                if !text.is_empty() && self.paste_target_matches(target) {
                    self.handle_paste(&text);
                }
            }
            Ok(None) => {}
            Err(e) => {
                if report_error {
                    self.push_notice((format!("clipboard: {e}"), "warning".into(), app_now()));
                }
            }
        }
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
                // fall back to reading the system clipboard for an image off the event loop
                if text.is_empty() {
                    let target = PasteTarget::MainEditor { clear_count: self.clear_count };
                    let app_tx = self.app_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let result = mare_binding::image::from_clipboard(2000)
                            .map(|img| img.map(ClipboardPaste::Image))
                            .map_err(|e| e.to_string());
                        let _ = app_tx.send(AppMsg::ClipboardPasteReady { target, report_error: false, result });
                    });
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
                self.ed().handle_paste(text);
                self.sync_editor_state();
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn repro_227_clipboard_paste_blocks_tui_event_loop_synchronously() {
        use std::net::TcpListener;
        use std::time::Duration;

        // Bind a local X11 display TCP port (6000 + N) that accepts a client and stalls 300ms
        // before closing, simulating a slow X11 clipboard owner without needing an external display.
        let (listener, display_num) = (100u16..200)
            .find_map(|n| TcpListener::bind(("127.0.0.1", 6000 + n)).ok().map(|l| (l, n)))
            .expect("bind local X11 display port");
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                if let Ok((stream, _)) = listener.accept() {
                    std::thread::sleep(Duration::from_millis(300));
                    drop(stream);
                }
            }
        });

        let prev_display = std::env::var_os("DISPLAY");
        let prev_wayland = std::env::var_os("WAYLAND_DISPLAY");
        unsafe {
            std::env::set_var("DISPLAY", format!("127.0.0.1:{display_num}"));
            std::env::remove_var("WAYLAND_DISPLAY");
        }
        struct RestoreEnv(Option<std::ffi::OsString>, Option<std::ffi::OsString>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(v) => std::env::set_var("DISPLAY", v),
                        None => std::env::remove_var("DISPLAY"),
                    }
                    match &self.1 {
                        Some(v) => std::env::set_var("WAYLAND_DISPLAY", v),
                        None => std::env::remove_var("WAYLAND_DISPLAY"),
                    }
                }
            }
        }
        let _restore = RestoreEnv(prev_display, prev_wayland);

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let tmp = tempfile::tempdir().unwrap();
                let opts = mare_host::HostOptions {
                    cwd: tmp.path().to_path_buf(),
                    mode: mare_shared::ui::Mode::Tui,
                    config: serde_json::json!({}),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({ "no_session": true }),
                    no_user_plugins: true,
                };
                let ui = UiShared::new();
                let host = mare_host::Host::with_ui(opts, ui.clone()).await.unwrap();
                let (app_tx, _app_rx) = tokio::sync::mpsc::unbounded_channel();
                let mut app = App::new(host.clone(), ui, app_tx, 80, 24);

                let t0 = std::time::Instant::now();
                app.handle_paste_at("", Instant::now());
                let empty_paste_elapsed = t0.elapsed();

                let t1 = std::time::Instant::now();
                let _ = app.paste_clipboard();
                let paste_action_elapsed = t1.elapsed();

                host.shutdown().await;
                assert!(
                    empty_paste_elapsed < Duration::from_millis(100)
                        && paste_action_elapsed < Duration::from_millis(100),
                    "clipboard paste must not block the TUI event loop synchronously \
                     (empty_paste={empty_paste_elapsed:?}, paste_clipboard={paste_action_elapsed:?})"
                );
            })
            .await;
        let _ = server.join();
    }

    #[tokio::test]
    async fn async_clipboard_paste_result_delivers_image_text_error_and_drops_stale_target() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let tmp = tempfile::tempdir().unwrap();
                let opts = mare_host::HostOptions {
                    cwd: tmp.path().to_path_buf(),
                    mode: mare_shared::ui::Mode::Tui,
                    config: serde_json::json!({}),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({ "no_session": true }),
                    no_user_plugins: true,
                };
                let ui = UiShared::new();
                let host = mare_host::Host::with_ui(opts, ui.clone()).await.unwrap();
                let (app_tx, _app_rx) = tokio::sync::mpsc::unbounded_channel();
                let mut app = App::new(host.clone(), ui, app_tx, 80, 24);
                let target = PasteTarget::MainEditor { clear_count: app.clear_count };

                // Text delivery inserts into the main editor.
                app.handle_clipboard_paste_result(target, true, Ok(Some(ClipboardPaste::Text("hello".into()))));
                assert_eq!(app.ed().text(), "hello");

                // Image delivery attaches an [Image #1] marker and stores the payload.
                app.handle_clipboard_paste_result(
                    target,
                    false,
                    Ok(Some(ClipboardPaste::Image(mare_binding::image::Encoded {
                        mime: "image/png",
                        data: "AAAA".into(),
                        width: 1,
                        height: 1,
                    }))),
                );
                assert!(app.ed().text().contains("[Image #1]"), "editor={:?}", app.ed().text());
                assert_eq!(app.attachments.len(), 1);

                // Stale target (clear_count moved) is ignored safely.
                app.clear_count += 1;
                app.handle_clipboard_paste_result(target, true, Ok(Some(ClipboardPaste::Text("stale".into()))));
                assert!(!app.ed().text().contains("stale"));

                // Error notice is pushed only when report_error is true.
                let notices_before = app.notices.iter().count();
                app.handle_clipboard_paste_result(target, false, Err("silent failure".into()));
                assert_eq!(app.notices.iter().count(), notices_before);
                app.handle_clipboard_paste_result(target, true, Err("visible failure".into()));
                assert_eq!(app.notices.iter().count(), notices_before + 1);

                host.shutdown().await;
            })
            .await;
    }
}
