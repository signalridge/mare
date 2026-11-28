//! Key routing: the main input's actions and key handling.

use super::prompt_history::HistNav;
use crate::app::*;

impl App {
    /// Whether the first active binding of a key is the requested core action.
    pub(super) fn key_selects_action(&self, chain: &[&str], key: &str, action: &str) -> bool {
        let state = if self.keymap.chain_needs_state(chain, key) { self.key_state() } else { 0 };
        self.keymap.chain_resolve(chain, key, state, false).0.first().is_some_and(|(_, name)| *name == action)
    }

    /// Run an action (core or Lua): false when it declined the key.
    pub(in crate::app) fn run_action(&mut self, action: &str) -> bool {
        self.run_action_on(action, None)
    }

    /// [`Self::run_action`] for a key pressed in a context (a Lua action gets `{key, context}`).
    pub(in crate::app) fn run_action_on(&mut self, action: &str, key: Option<(&str, &str)>) -> bool {
        match action {
            // Esc: a completion menu only closes (row 7); `!` with nothing after it
            // leaves shell mode (row 8 onEscape); otherwise the plugins
            // decide — interrupt the running work, or the double-Esc /undo picker when idle
            "interrupt" => {
                if self.completion.is_open() {
                    self.completion.invalidate();
                } else if self.working.is_none() && matches!(self.ed().raw_text().trim(), "!" | "!!") {
                    self.ed().clear();
                } else {
                    self.interrupt();
                }
            }
            "transcript" => self.transcript_overlay(),
            // all key bindings, searchable; `?`
            // runs it on an empty input (its default binding's `when`)
            "keymap-help" => self.open_keymap_help(),
            "toggle-expand" => {
                // Ctrl+O: toggle compact/expanded tool output and thinking across both committed
                // scrollback (via debounced reflow) and live blocks.
                self.expand_all = !self.expand_all;
                self.transcript.invalidate_cache();
                self.reflow.force_invalidated(app_now());
                let mode = if self.expand_all { "expanded" } else { "compact" };
                self.push_notice_with_id(
                    (format!("Tool output: {mode}"), "info".into(), app_now()),
                    Some("toggle-expand".into()),
                );
            }
            // Ctrl+D: nothing with a menu open (row 7); with a draft the editor
            // deletes the character after the cursor (rows 2, 11); while working it never exits
            // (rows 3, 4, 9 exits here, mare doesn't); on an empty input it exits
            "quit-if-empty" => {
                if self.completion.is_open() {
                    return true;
                }
                if !self.ed().is_empty() {
                    return false;
                }
                if self.working.is_some() {
                    self.push_notice(("Ctrl+C to stop".into(), "info".into(), Instant::now()));
                } else {
                    self.request_quit();
                }
            }
            "quit" => self.request_quit(),
            "suspend" if cfg!(windows) => {
                let label = key
                    .map(|(k, _)| format!("{} (suspend)", mare_shared::bindings::key_label(k)))
                    .unwrap_or_else(|| "Suspend".to_string());
                self.push_notice((format!("{label} is not supported on Windows"), "info".into(), Instant::now()))
            }
            "suspend" => self.suspend_requested = true,
            // select mode: the mouse selects text in the live area, release copies
            "select-mode" => {
                if self.select.active() {
                    self.leave_select_mode();
                } else {
                    self.select.enter();
                    mare_tui::modes::set_mouse(true);
                }
            }
            "redraw" => {
                if self.transcript.is_open() {
                    self.transcript.frame.clear();
                } else {
                    self.rebuild_history();
                }
            }
            // Ctrl+S (steer: into the running turn) / Alt+Enter (follow-up): only a prompt goes to
            // the agent's queues; a slash command or a `!cmd` is no message for the model and
            // runs as Enter runs it; pasted images travel with either
            "steer" => self.send_special("input/steer"),
            "history-search" => self.history_search(),
            "follow-up" => self.send_special("input/follow-up"),
            "complete" => {
                if self.completion.is_open() && self.completion_matches_current_token() {
                    if let Some(c) = self.completion.popup.take() {
                        self.completion.invalidate();
                        self.accept_completion(c);
                    }
                } else {
                    self.completion.invalidate();
                    self.request_completion(true);
                }
            }
            "newline" => {
                let act = self.ed().apply(mare_tui::editor::TextOp::Newline);
                return self.apply_editor_action(act);
            }
            "external-editor" => self.external_editor_requested = true,
            "paste-image" => return self.paste_clipboard(),
            "text.up" | "text.down" if self.completion.is_open() => {
                let c = self.completion.popup.as_mut().expect("open completion");
                c.selected = if action == "text.up" {
                    c.selected.checked_sub(1).unwrap_or(c.items.len().saturating_sub(1))
                } else {
                    (c.selected + 1) % c.items.len().max(1)
                };
            }
            "editor.submit" => {
                if self.modal.is_some() {
                    return false;
                }
                if self.completion.is_open() {
                    if self.completion_matches_current_token() {
                        if let Some(c) = self.completion.popup.take() {
                            self.completion.invalidate();
                            let auto =
                                c.items.get(c.selected).is_some_and(|i| i.insert.starts_with('/') && i.auto_execute);
                            self.accept_completion(c);
                            if auto && self.ed().raw_text().split_whitespace().count() == 1 {
                                let (text, elements) = self.ed().text_with_elements();
                                self.ed().set_submitted_elements(elements);
                                self.ed().clear();
                                self.submit(text.trim_end().to_string());
                            }
                            return true;
                        }
                    } else {
                        self.completion.invalidate();
                    }
                }
                let act = self.ed().enter();
                return self.apply_editor_action(act);
            }
            "input.accept" => {
                let Some(Modal::Input { editor, reply, .. }) = self.modal.as_mut() else {
                    return false;
                };
                let (t, _) = editor.text_with_elements();
                if let Some(r) = reply.take() {
                    let _ = r.send(Ok(t));
                }
                self.pop_modal_or_promote_delayed();
            }
            "input.cancel" => {
                let Some(Modal::Input { reply, .. }) = self.modal.as_mut() else {
                    return false;
                };
                if let Some(r) = reply.take() {
                    let _ = r.send(Err(Dismissed::Cancelled));
                }
                self.pop_modal_or_promote_delayed();
            }
            other => {
                if let Some(op) = mare_tui::editor::TextOp::parse(other) {
                    if let Some(Modal::Input { editor, .. }) = self.modal.as_mut() {
                        return editor.apply(op) != EditorAction::Unhandled;
                    }
                    if self.modal.is_some() {
                        return false;
                    }
                    let act = self.ed().apply(op);
                    return self.apply_editor_action(act);
                }
                return self
                    .modal_action(other)
                    .or_else(|| self.view_action(other))
                    .unwrap_or_else(|| self.lua_action(other, key));
            }
        }
        true
    }

    /// Context lookup chain when the main editor has focus.
    pub(in crate::app) fn editor_chain(&self, select_active: bool) -> Vec<String> {
        let mut chain = Vec::with_capacity(8);
        if self.completion.is_open() {
            chain.push("editor.completion".into());
        }
        if self.ui.queue_mode.load(std::sync::atomic::Ordering::Relaxed) != mare_tui::queue_keys::OFF {
            chain.push("editor.queue".into());
        }
        if self.history.browsing() {
            chain.push("editor.history".into());
        }
        if select_active {
            chain.push("editor.selection".into());
        }
        chain.extend(self.keymap.active_subcontexts_for("editor"));
        chain.push("editor".into());
        chain.push("text".into());
        chain.push("global".into());
        chain
    }

    /// Active context lookup chain for the focused editor (`select` mode or text selection).
    pub(in crate::app) fn current_editor_chain(&self) -> Vec<String> {
        let has_sel = self.ed().selection().is_some();
        self.editor_chain(self.select.active() || has_sel)
    }

    /// Start a key sequence when `ks` is a bound prefix along `chain`.
    pub(in crate::app) fn try_prefix(&mut self, chain: &[&str], ks: &str, focus: KeyFocus) -> bool {
        let state = self.key_state();
        if self.keymap.is_prefix_for_state(chain, ks, state) {
            self.pending_prefix = Some((ks.to_string(), focus));
            true
        } else {
            false
        }
    }

    /// Consume a pending sequence prefix: the focused context's cancel binding cancels it.
    /// Returns `true` if the key was consumed by an active prefix matching `expected` focus
    /// (including extending into a deeper multi-key prefix chain).
    pub(in crate::app) fn complete_prefix(
        &mut self,
        expected: KeyFocus,
        _key: &KeyEvent,
        ks: &str,
        chain: &[&str],
    ) -> bool {
        let Some((p, focus)) = self.pending_prefix.take() else {
            return false;
        };
        if focus != expected {
            return false;
        }
        let cancel_action = match expected {
            KeyFocus::Editor => "interrupt",
            KeyFocus::Transcript => "view.back",
            KeyFocus::Select => "select.back",
        };
        if self.key_selects_action(chain, ks, cancel_action) {
            return true;
        }
        let seq = format!("{p} {ks}");
        let state = self.key_state();
        if self.keymap.is_prefix_for_state(chain, &seq, state) {
            self.pending_prefix = Some((seq, expected));
            return true;
        }
        self.run_bindings(chain, &seq, false);
        true
    }

    pub(in crate::app) fn handle_key(&mut self, key: KeyEvent) {
        self.handle_key_at(key, mare_tui::render::clock_now());
    }

    pub(in crate::app) fn handle_key_at(&mut self, key: KeyEvent, at: Instant) {
        self.handle_key_inner(key, at);
        self.sync_editor_state();
    }

    fn handle_key_inner(&mut self, key: KeyEvent, at: Instant) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if mare_tui::keys::is_ctrl_c(&key) {
            self.handle_ctrl_c_at(at);
            return;
        }
        self.disarm_quit();
        self.keymap.sync_from(&self.ui.keymap.read().unwrap());
        // an overlay (the transcript, a child's view) with a modal over it: the modal takes the
        // keys, Esc included; the overlay stays open underneath
        if self.transcript.is_open() {
            match self.modal {
                Some(_) => self.handle_modal_key_at(key, at),
                None => self.handle_transcript_key(key),
            }
            return;
        }
        if self.handle_paste_burst_key(key, at) {
            return;
        }
        if self.modal.is_some() {
            self.handle_modal_key_at(key, at);
            return;
        }
        let ks = key_to_string(&key);
        // The interrupt binding leaves select mode; other keys leave it and keep their normal action.
        let select_active = self.select.active();
        if select_active {
            let chain = self.current_editor_chain();
            let refs: Vec<&str> = chain.iter().map(String::as_str).collect();
            let cancel = self.key_selects_action(&refs, &ks, "interrupt");
            self.leave_select_mode();
            if cancel {
                return;
            }
        }
        let chain = self.current_editor_chain();
        let chain_refs: Vec<&str> = chain.iter().map(String::as_str).collect();
        // which-key (Yazi): after a prefix of multi-key bindings (`ctrl+x` of `ctrl+x g`), the next
        // key completes the sequence; Esc or an unbound key cancels
        if self.complete_prefix(KeyFocus::Editor, &key, &ks, &chain_refs) {
            return;
        }
        if self.try_prefix(&chain_refs, &ks, KeyFocus::Editor) {
            return;
        }
        // Editor/text bindings and the line editor's kernel keys take priority over `global` so
        // `global` bindings (e.g. `<Up>`) cannot steal editor navigation or typing.
        let (before_global, has_global) = match chain_refs.last() {
            Some(&"global") => (&chain_refs[..chain_refs.len() - 1], true),
            _ => (&chain_refs[..], false),
        };
        let prev_rev = self.ed().text_revision();
        let printable =
            mare_tui::keys::printable_char(&key).is_some() || mare_shared::bindings::is_printable_key_name(&ks);
        let mut handled_in_editor = false;
        if self.dispatch_editor_bindings_stops(before_global, &ks) {
            handled_in_editor = true;
        } else if printable && self.keymap.should_swallow_printable(before_global) {
            handled_in_editor = true;
        } else if self.handle_editor_key(key) {
            handled_in_editor = true;
        }
        let is_enter = ks == "enter" || ks.ends_with("+enter");
        if self.ed().text_revision() != prev_rev || printable || is_enter {
            self.record_editor_activity(app_now());
        }
        if handled_in_editor {
            return;
        }
        if has_global && !mare_shared::bindings::is_printable_key_name(&ks) {
            self.run_bindings(&["global"], &ks, false);
        }
    }

    /// The reserved interrupt key acts on the topmost focus before any keymap lookup.
    fn handle_ctrl_c_at(&mut self, now: Instant) {
        if let Some(modal) = self.modal.take() {
            self.disarm_quit();
            if matches!(self.pending_prefix, Some((_, KeyFocus::Select))) {
                self.pending_prefix = None;
            }
            match modal {
                Modal::Select { reply: Some(reply), .. } => {
                    let _ = reply.send(Err(Dismissed::Interrupted));
                }
                Modal::Input { reply: Some(reply), .. } => {
                    let _ = reply.send(Err(Dismissed::Interrupted));
                }
                _ => {}
            }
            self.pop_modal_or_promote_delayed();
            return;
        }
        if self.pending_prefix.take().is_some() {
            self.disarm_quit();
            return;
        }
        if self.transcript.is_open() {
            self.disarm_quit();
            if !self.transcript.cancel_typing_search() {
                self.close_transcript();
            }
            return;
        }
        if self.completion.is_open() {
            self.disarm_quit();
            self.completion.invalidate();
            return;
        }
        if self.select.active() {
            self.disarm_quit();
            if self.copy_selection() {
                return;
            }
            self.leave_select_mode();
        }
        let prev_qmode = self.cancel_queue_mode(true);
        if prev_qmode != mare_tui::queue_keys::OFF {
            if prev_qmode == mare_tui::queue_keys::EDITING && !self.ed().is_empty() {
                self.clear_draft();
            }
            return;
        }
        if self.stopping() {
            self.disarm_quit();
            mare_binding::stopping::force();
            self.push_notice(("Force-stopping (SIGKILL)".into(), "warning".into(), now));
        } else if !self.ed().is_empty() {
            self.disarm_quit();
            self.clear_draft();
        } else if self.working.is_some() {
            self.disarm_quit();
            self.interrupt();
        } else if self.quit_armed.is_some_and(|t| now.saturating_duration_since(t) < QUIT_WINDOW) {
            self.disarm_quit();
            self.request_quit();
        } else {
            self.quit_armed = Some(now);
            self.push_notice_for_id(QUIT_HINT, "info", QUIT_WINDOW, QUIT_NOTICE_ID);
        }
    }

    /// Pastes the terminal didn't bracket arrive as fast key streams: an Enter inside one is a
    /// newline, not a submit; the Enter Windows Terminal sends right after
    /// a bracketed paste is dropped. Timed by arrival timestamp (`now`), not processing time.
    fn handle_paste_burst_key(&mut self, key: KeyEvent, now: Instant) -> bool {
        let plain = !key.modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT);
        let is_approval = self.is_approval_modal();
        match key.code {
            KeyCode::Enter if key.modifiers.is_empty() => {
                if self.paste_guard.swallow_enter(now) {
                    return true;
                }
                if self.burst_enabled
                    && (self.burst.enter_is_newline(now) || (is_approval && self.burst.in_burst_sequence(now)))
                {
                    if self.modal.is_none() {
                        self.ed().insert_str("\n");
                        self.record_editor_activity(now);
                    }
                    return true;
                }
            }
            KeyCode::Char(_) if plain => {
                self.burst.on_char(now);
                self.paste_guard.clear();
                if self.burst_enabled && is_approval && self.burst.in_burst_sequence(now) {
                    return true;
                }
            }
            _ => {
                self.burst.clear();
                self.paste_guard.clear();
            }
        }
        false
    }

    /// Queued messages: ↑ selects one, Enter edits it in place (`queue_keys`).
    pub(super) fn route_queue_action(&mut self, action: Option<&str>) -> bool {
        let empty = self.ed().is_empty();
        if self.queue_edit_loading && action == Some("editor.submit") && empty {
            return true;
        }
        match mare_tui::queue_keys::route(
            self.ui.queue_mode.load(std::sync::atomic::Ordering::Relaxed),
            self.ui.queued.load(std::sync::atomic::Ordering::Relaxed),
            empty,
            self.history.browsing(),
            self.completion.is_open(),
            action,
        ) {
            mare_tui::queue_keys::Route::Pass => false,
            mare_tui::queue_keys::Route::LeaveThenPass => {
                self.cancel_queue_mode(true);
                false
            }
            mare_tui::queue_keys::Route::Emit { event, delta, mode } => {
                self.queue_edit_loading = event == "input/queue-edit";
                self.ui.queue_mode.store(mode, std::sync::atomic::Ordering::Relaxed);
                self.emit(event, delta.map_or_else(|| serde_json::json!({}), |d| serde_json::json!({ "delta": d })));
                true
            }
        }
    }

    fn apply_editor_action(&mut self, act: EditorAction) -> bool {
        match act {
            EditorAction::Submit(text) => self.submit(text),
            EditorAction::Changed => {
                self.history.reset();
                self.request_completion(false);
            }
            EditorAction::HistoryPrev => {
                let raw = self.ed().raw_text();
                if let Some(text) = self.history.prev(|| raw) {
                    if self.ui.queue_mode.load(std::sync::atomic::Ordering::Relaxed) == mare_tui::queue_keys::EDITING {
                        self.cancel_queue_mode(true);
                    }
                    self.ed().recall(&text, true);
                }
            }
            EditorAction::HistoryNext => match self.history.next() {
                HistNav::Recall(text) => {
                    if self.ui.queue_mode.load(std::sync::atomic::Ordering::Relaxed) == mare_tui::queue_keys::EDITING {
                        self.cancel_queue_mode(true);
                    }
                    self.ed().recall(&text, false);
                }
                HistNav::Draft(d) => {
                    if self.ui.queue_mode.load(std::sync::atomic::Ordering::Relaxed) == mare_tui::queue_keys::EDITING {
                        self.cancel_queue_mode(true);
                    }
                    self.ed().set_text(&d);
                }
                HistNav::Stay => {}
            },
            EditorAction::None => {}
            EditorAction::Unhandled => return false,
        }
        true
    }

    fn handle_editor_key(&mut self, key: KeyEvent) -> bool {
        let Some(c) = mare_tui::keys::printable_char(&key) else {
            return false;
        };
        let act = self.ed().insert_char(c);
        self.apply_editor_action(act)
    }
}
