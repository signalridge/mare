//! UI commands from plugins.

use crate::app::*;

impl App {
    // --------------------------------------------------------------
    // UI commands from plugins
    // --------------------------------------------------------------
    pub(in crate::app) fn handle_cmd(&mut self, cmd: UiCmd) {
        match cmd {
            UiCmd::BlockPush { .. }
            | UiCmd::BlockUpdate { .. }
            | UiCmd::BlockFinish { .. }
            | UiCmd::BlockRemove { .. }
            | UiCmd::Print { .. }
            | UiCmd::Clear => self.handle_block_cmd(cmd),
            UiCmd::Select { .. }
            | UiCmd::SelectUpdate { .. }
            | UiCmd::SelectPreview { .. }
            | UiCmd::Input { .. }
            | UiCmd::CancelModal { .. }
            | UiCmd::Overlay { .. }
            | UiCmd::TranscriptView { .. }
            | UiCmd::TranscriptClose { .. } => self.handle_modal_cmd(cmd),
            UiCmd::SetEditor { .. }
            | UiCmd::EditorConfig { .. }
            | UiCmd::EditorHint { .. }
            | UiCmd::SetBindings { .. }
            | UiCmd::SetBindingsBatch { .. }
            | UiCmd::SetContext { .. }
            | UiCmd::SetContextActive { .. }
            | UiCmd::EditorOp(_)
            | UiCmd::SyncEditorChanged
            | UiCmd::CompletionSourceRemoved { .. } => self.handle_editor_cmd(cmd),
            UiCmd::Flag { name, value } => match name.as_str() {
                "show_reasoning" => {
                    if self.show_reasoning != value {
                        self.show_reasoning = value;
                        self.transcript.invalidate_cache();
                        self.reflow.invalidated(app_now());
                    }
                }
                "expand_all" => {
                    if self.expand_all != value {
                        self.expand_all = value;
                        self.transcript.invalidate_cache();
                        self.reflow.invalidated(app_now());
                    }
                }
                // spinners hold still (static marker), no animation frames
                "reduce_motion" => self.reduce_motion = value,
                _ => {}
            },
            UiCmd::Widget { id, slot, order, node } => {
                self.widgets.retain(|k, _| k.2 != id);
                if let Some(n) = node {
                    let has_anim = n.has_animation();
                    self.widgets.insert((slot, order, id), Widget { node: n, has_anim });
                }
            }
            UiCmd::Status { id, align, order, node, keep } => {
                self.status.retain(|k, _| k.3 != id);
                let seq = *self.status_known_seq.entry(id.clone()).or_insert_with(|| {
                    let s = self.status_seq;
                    self.status_seq = self.status_seq.saturating_add(1);
                    s
                });
                if let Some(n) = node {
                    let has_anim = n.has_animation();
                    self.status.insert((align, order, seq, id), StatusSegment { node: n, keep, has_anim });
                }
            }
            UiCmd::Working { label, detail, background } => match label {
                Some(l) => {
                    let started = self.working.as_ref().map(|w| w.2).unwrap_or_else(mare_tui::render::clock_now);
                    self.working = Some((l, detail, started, background));
                }
                None => self.working = None,
            },
            UiCmd::Notify { text, level, id } => self.push_notice_with_id((text, level, app_now()), id),
            UiCmd::ThemeChanged => self.reflow.invalidated(app_now()),
            UiCmd::Redraw => {}
            UiCmd::ColorMode(mode) => {
                self.renderer.color = mode;
                {
                    let mut caps = self.ui.caps.write().unwrap();
                    if let Some(obj) = caps.as_object_mut() {
                        obj.insert("color_depth".into(), serde_json::json!(mode.as_str()));
                        obj.insert("truecolor".into(), serde_json::json!(mode == ColorMode::TrueColor));
                        obj.insert("no_color".into(), serde_json::json!(mode == ColorMode::NoColor));
                    }
                }
                mare_shared::style::theme_changed();
                self.reflow.invalidated(app_now());
            }
            UiCmd::Exit => self.request_quit(),
            UiCmd::ExitForce => self.quit = true,
            UiCmd::ExitGuard { on } => self.exit_guard = on,
            UiCmd::Layout(cfg) => self.layout = cfg,
            UiCmd::Title { text } => {
                if mare_tui::modes::handed_off() {
                    self.deferred_title = Some(text);
                } else {
                    // OSC 2 from the write exit: the text is cleaned as a label
                    mare_tui::out::title(&text);
                }
            }
            UiCmd::NotifyOs { text, protocol, bell } => {
                // Windows: a terminal that can't show OSC notifications (legacy conhost, anything
                // but Windows Terminal) gets a native balloon/toast instead; "native" forces it
                if native_notification(&protocol) && mare_binding::platform::notify_native("mare", &text) {
                    return;
                }
                if mare_tui::modes::handed_off() {
                    return;
                }
                // OSC 9 / 777 / 99 with tmux & screen passthrough; BEL as a fallback nudge
                mare_tui::out::notify("mare", &text, &protocol, bell);
            }
        }
    }

    fn handle_block_cmd(&mut self, cmd: UiCmd) {
        match cmd {
            UiCmd::BlockPush { id, kind, patch } => {
                let mut b = Block {
                    id,
                    kind,
                    node: None,
                    has_anim: false,
                    title: None,
                    md: None,
                    expanded: None,
                    expanded_has_anim: false,
                    finished: false,
                    started: Instant::now(),
                    ended: None,
                    partial: 0,
                    sc: Default::default(),
                };
                b.apply(patch);
                self.blocks.push(b);
            }
            UiCmd::BlockUpdate { id, patch } => {
                if let Some(index) = self.blocks.iter().rposition(|block| block.id == id) {
                    let block = &mut self.blocks[index];
                    let replaces_layout = patch.node.is_some()
                        || patch.markdown.is_some()
                        || patch.expanded.is_some()
                        || patch.kind.is_some();
                    let replaces_partial = block.partial > 0 && replaces_layout;
                    block.apply(patch);
                    if replaces_partial {
                        // A source-based stream cursor cannot index a replacement component's
                        // rows. Replay prior history and keep this entire component live.
                        block.partial = 0;
                        block.sc = Default::default();
                    }
                    if index < self.committed || replaces_partial {
                        // Late asynchronous presentation updates use the same bounded,
                        // debounced history replay as resize/theme changes.
                        self.transcript.invalidate_cache();
                        self.reflow.invalidated(Instant::now());
                    }
                }
            }
            UiCmd::BlockFinish { id } => {
                if let Some(b) = self.blocks.iter_mut().rev().find(|b| b.id == id) {
                    b.finished = true;
                    b.ended.get_or_insert_with(Instant::now);
                }
            }
            UiCmd::BlockRemove { id } => {
                self.image_hold.clear_if(id);
                if let Some(pos) = self.blocks.iter().position(|b| b.id == id)
                    && pos >= self.committed
                {
                    self.blocks.remove(pos);
                }
            }
            UiCmd::Print { node } => {
                let id = self.ui.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let has_anim = node.has_animation();
                self.blocks.push(Block {
                    id,
                    kind: "print".into(),
                    node: Some(node),
                    has_anim,
                    title: None,
                    md: None,
                    expanded: None,
                    expanded_has_anim: false,
                    finished: true,
                    started: Instant::now(),
                    ended: None,
                    partial: 0,
                    sc: Default::default(),
                });
            }
            UiCmd::Clear => {
                self.clear_count += 1;
                self.image_hold.clear();
                self.blocks.clear();
                self.committed = 0;
                if self.transcript.is_open() {
                    self.transcript.frame.clear();
                    self.transcript.invalidate_cache();
                    self.reflow.invalidated(app_now());
                } else if mare_tui::modes::handed_off() {
                    self.deferred_clear = true;
                } else {
                    self.renderer.clear_screen();
                }
            }
            _ => unreachable!("non-block UiCmd routed to handle_block_cmd"),
        }
        self.transcript.invalidate_anim();
        self.transcript.invalidate_search();
    }

    pub(in crate::app) fn push_overlay_under_modal(&mut self) {
        if let Some(m) = self.modal.take() {
            self.modal_stack.push(m);
        }
    }

    fn handle_modal_cmd(&mut self, cmd: UiCmd) {
        match cmd {
            UiCmd::Select {
                title,
                title_right,
                split,
                mut items,
                selected,
                multi,
                accept_label,
                allow_search,
                filter,
                mut tabs,
                tab,
                kind,
                keys,
                body,
                vim,
                highlight,
                reply,
                token,
                modal_id,
                approval,
            } => {
                // a search given up front is a search: the first Esc clears it
                let searching = allow_search && !filter.is_empty();
                // with an initial filter, start at the best match
                let mut selected = if filter.is_empty() { selected } else { 0 };
                let tab = tab.min(tabs.len().saturating_sub(1));
                if let Some(t) = tabs.get_mut(tab) {
                    items = std::mem::take(&mut t.items);
                    selected = t.selected;
                }
                // never start on a group heading
                selected = skip_unselectable(&items, &filter_items(&items, &filter), selected, true);
                let modal = Modal::Select {
                    title,
                    title_right,
                    split,
                    items,
                    filter,
                    selected,
                    multi,
                    accept_label,
                    allow_search,
                    tabs,
                    tab,
                    kind,
                    keys,
                    body,
                    body_scroll: 0,
                    body_scroll_max: None,
                    preview_scroll: 0,
                    preview_scroll_max: None,
                    preview_hidden: false,
                    searching,
                    vim,
                    count: 0,
                    highlight,
                    last_hl: None,
                    reply: Some(reply),
                    token,
                    modal_id,
                    list_rev: 0,
                    filter_cache: SelectFilterCache::default(),
                    approval,
                    drawn: false,
                };
                let now = app_now();
                if approval
                    && (!self.delayed_approvals.is_empty()
                        || self.modal.is_some()
                        || self.prompt_delay_remaining(now).is_some())
                {
                    self.delayed_approvals.push_back(modal);
                } else {
                    self.activate_select_modal(modal);
                }
            }
            UiCmd::SelectUpdate { token, items: new, tabs: new_tabs } => {
                let mut updated_top = false;
                if let Some(Modal::Select {
                    token: t,
                    items,
                    filter,
                    selected,
                    tabs,
                    tab,
                    last_hl,
                    list_rev,
                    filter_cache,
                    ..
                }) = self.modal.as_mut()
                    && *t == token
                    && token != 0
                {
                    super::select_update::apply(items, selected, tabs, tab, filter, new, new_tabs);
                    *list_rev += 1;
                    filter_cache.bump();
                    *last_hl = None;
                    updated_top = true;
                } else if token != 0
                    && let Some(Modal::Select {
                        items,
                        filter,
                        selected,
                        tabs,
                        tab,
                        last_hl,
                        list_rev,
                        filter_cache,
                        ..
                    }) = self
                        .modal_stack
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Modal::Select { token: t, .. } if *t == token))
                {
                    super::select_update::apply(items, selected, tabs, tab, filter, new, new_tabs);
                    *list_rev += 1;
                    filter_cache.bump();
                    *last_hl = None;
                }
                if updated_top {
                    self.notify_highlight();
                }
                self.reset_select_row_failures();
            }
            UiCmd::SelectPreview { modal_id, revision, tab: t, index, node } => {
                let target = if matches!(self.modal, Some(Modal::Select { modal_id: mid, list_rev, .. }) if mid == modal_id && list_rev == revision)
                {
                    self.modal.as_mut()
                } else {
                    self.modal_stack.iter_mut().rev().find(|m| {
                        matches!(m, Modal::Select { modal_id: mid, list_rev, .. } if *mid == modal_id && *list_rev == revision)
                    })
                };
                if let Some(Modal::Select { items, tabs, tab, .. }) = target {
                    let list = if t == *tab { Some(items) } else { tabs.get_mut(t).map(|x| &mut x.items) };
                    if let Some(it) = list.and_then(|l| l.get_mut(index)) {
                        it.preview = Some(node);
                    }
                }
            }
            UiCmd::Input { title, placeholder, default, secret, reply, modal_id } => {
                if self.select.active() {
                    self.leave_select_mode();
                }
                let mut editor = Editor::new();
                if !default.is_empty() {
                    editor.set_text(&default);
                }
                self.disarm_quit();
                self.push_overlay_under_modal();
                self.modal = Some(Modal::Input { title, editor, secret, placeholder, reply: Some(reply), modal_id });
            }
            UiCmd::CancelModal { modal_id } if modal_id != 0 => {
                self.delayed_approvals.retain(|m| m.modal_id() != Some(modal_id));
                let mut popped = false;
                if self.modal.as_ref().and_then(Modal::modal_id) == Some(modal_id) {
                    self.pop_modal_or_promote_delayed();
                    popped = true;
                } else if matches!(self.modal, Some(Modal::Help { .. }))
                    && self.modal_stack.iter().any(|m| m.modal_id() == Some(modal_id))
                {
                    self.modal_stack.retain(|m| m.modal_id() != Some(modal_id));
                    self.pop_modal_or_promote_delayed();
                    popped = true;
                } else {
                    self.modal_stack.retain(|m| m.modal_id() != Some(modal_id));
                }
                if popped {
                    self.notify_highlight();
                }
            }
            UiCmd::CancelModal { .. } => {}
            UiCmd::Overlay { title, node, context, cursor } => {
                self.disarm_quit();
                if self.select.active() {
                    self.leave_select_mode();
                }
                let info = Modal::Info { title, node, scroll: 0, scroll_max: None, context, cursor };
                if self.modal.is_none() || matches!(self.modal, Some(Modal::Info { .. })) {
                    self.modal = Some(info);
                } else if let Some(slot) = self.modal_stack.iter_mut().rev().find(|m| matches!(m, Modal::Info { .. })) {
                    *slot = info;
                } else {
                    self.modal_stack.insert(0, info);
                }
            }
            UiCmd::TranscriptView { title, node, keys } => self.show_plugin_transcript(title, node, keys),
            UiCmd::TranscriptClose { title } => self.close_plugin_transcript(title),
            _ => unreachable!("non-modal UiCmd routed to handle_modal_cmd"),
        }
    }

    pub(in crate::app) fn handle_editor_cmd(&mut self, cmd: UiCmd) {
        if let Some(session) = self.external_editor.as_mut()
            && matches!(cmd, UiCmd::SetEditor { .. } | UiCmd::EditorOp(_))
        {
            session.pending_editor_cmds.push(cmd);
            return;
        }
        match cmd {
            UiCmd::SetEditor { text, images, image_markers, keep_draft, prepend } => {
                let empty = self.ed().is_empty();
                let (text, restored_elements) = if prepend || !keep_draft || empty {
                    self.restore_images(&text, images, image_markers)
                } else {
                    (text, Vec::new())
                };
                if prepend {
                    crate::not_sent::restore(&mut self.ed(), &text);
                    for (range, marker, image) in restored_elements {
                        let registered = self.ed().register_element_in_text(&text, range, &marker);
                        if let Some(id) = registered {
                            self.attachments.push((id, marker, image));
                        }
                    }
                    self.sync_editor_state();
                } else if keep_draft && !empty {
                    // the draft wins; say where the message text would have gone
                    if !text.is_empty() {
                        self.push_notice((
                            "Kept your draft (the message text was not put in the input)".into(),
                            "info".into(),
                            Instant::now(),
                        ));
                    }
                } else {
                    if self.queue_edit_loading {
                        self.queue_edit_loading = false;
                    } else if self.ui.queue_mode.load(std::sync::atomic::Ordering::Relaxed)
                        == mare_tui::queue_keys::EDITING
                    {
                        self.cancel_queue_mode(true);
                    }
                    self.ed().set_text(&text);
                    let live = self.ed().referenced_element_ids();
                    self.attachments.retain(|(id, _, _)| live.contains(id));
                    for (range, marker, image) in restored_elements {
                        let registered = self.ed().register_element_in_text(&text, range, &marker);
                        if let Some(id) = registered {
                            self.attachments.push((id, marker, image));
                        }
                    }
                    self.sync_editor_state();
                }
            }
            UiCmd::EditorConfig { prompt, placeholder, busy_placeholder, border, shell } => {
                if let Some(p) = prompt {
                    self.editor_prompt = p;
                }
                if placeholder.is_some() {
                    self.editor_placeholder = placeholder;
                }
                if busy_placeholder.is_some() {
                    self.editor_busy_placeholder = busy_placeholder;
                }
                if border.is_some() {
                    self.editor_border = border;
                }
                match shell {
                    Some(Some(style)) => {
                        self.ed().enable_shell_mode();
                        self.shell_style = Some(style);
                    }
                    Some(None) => {
                        self.ed().disable_shell_mode();
                        self.shell_style = None;
                    }
                    None => {}
                }
                self.sync_editor_state();
            }
            UiCmd::EditorHint { text } => self.hint = if text.is_empty() { None } else { Some(text) },
            UiCmd::SetBindings { .. }
            | UiCmd::SetBindingsBatch { .. }
            | UiCmd::SetContext { .. }
            | UiCmd::SetContextActive { .. } => {
                self.keymap.sync_from(&self.ui.keymap.read().unwrap());
            }
            UiCmd::EditorOp(op) => {
                let _ = self.editor.apply(op);
                self.sync_editor_state();
            }
            UiCmd::SyncEditorChanged => {
                self.sync_editor_state();
            }
            UiCmd::CompletionSourceRemoved { id } => {
                if self.completion.hint.as_ref().is_some_and(|(source, _)| *source == id) {
                    self.completion.hint = None;
                }
                if let Some(c) = &mut self.completion.popup {
                    c.items.retain(|it| it.source_id != Some(id));
                    if c.items.is_empty() {
                        self.completion.popup = None;
                    } else {
                        c.selected = c.selected.min(c.items.len() - 1);
                    }
                }
            }
            _ => unreachable!("non-editor UiCmd routed to handle_editor_cmd"),
        }
    }
}

/// Whether an OS notification goes to the desktop instead of the terminal.
fn native_notification(protocol: &str) -> bool {
    match protocol {
        "native" => true,
        "auto" | "" => {
            cfg!(windows)
                && crate::terminal::detect(&|k| std::env::var(k).ok()) != crate::terminal::TermKind::WindowsTerminal
        }
        _ => false,
    }
}
