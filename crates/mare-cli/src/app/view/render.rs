//! Per-frame composition: live area, modals, cards, status bar.

use super::notices::icon_for;
use super::selection::highlight_selection;
use crate::app::*;
use mare_shared::layout::{RegionLines, StackRegion, clip_regions};

impl App {
    pub(super) fn editor_action_key(&self, action: &str) -> Option<String> {
        let chain = self.current_editor_chain();
        let chain: Vec<&str> = chain.iter().map(String::as_str).collect();
        self.keymap.hint(action, &chain).first().map(|key| mare_shared::bindings::key_label(key))
    }
    pub(super) fn modal_has_animation(&self) -> bool {
        match self.modal.as_ref() {
            Some(Modal::Select { kind, .. }) if kind == "confirm" => self.component_caches.confirm_has_anim.get(),
            Some(m) => m.has_animation(),
            None => false,
        }
    }

    pub(in crate::app) fn image_hold_deadline(&self) -> Option<Instant> {
        if self.transcript.is_open() {
            return None;
        }
        let front = self.blocks.get(self.committed).filter(|b| b.finished).map(|b| b.id);
        self.image_hold.deadline_for(front)
    }

    pub(in crate::app) fn animating(&self) -> bool {
        if self.transcript.is_open() {
            return !self.reduce_motion && (self.transcript_has_visible_animation() || self.modal_has_animation());
        }
        self.working.is_some()
            || self.stopping()
            || (!self.reduce_motion
                && (self.blocks[self.committed..]
                    .iter()
                    .any(|b| b.has_animation(self.expand_all, self.show_reasoning))
                    || self.widgets.iter().any(|((slot, _, _), w)| {
                        w.has_anim
                            && match slot.as_str() {
                                "above-editor" => !self.layout.is_hidden(StackRegion::AboveEditor),
                                "below-editor" => !self.layout.is_hidden(StackRegion::BelowEditor),
                                _ => true,
                            }
                    })
                    || self.status.values().any(|s| s.has_anim)
                    || self.modal_has_animation()))
    }

    /// under a running
    /// shell command, the key that moves it to the background. Inside tmux Ctrl+B is the prefix:
    /// tmux passes it on only when pressed twice (prefix, prefix), so the hint says so and names
    /// the conflict.
    fn background_hint(&self, background: bool) -> Option<String> {
        if !background {
            return None;
        }
        let chain = self.current_editor_chain();
        let chain_refs: Vec<&str> = chain.iter().map(String::as_str).collect();
        let key = *self.keymap.hint("background", &chain_refs).first()?;
        Some(if self.in_tmux && key == "ctrl+b" {
            let label = mare_shared::bindings::key_label(key);
            format!("{label} {label} to run in background (tmux prefix)")
        } else {
            format!("{} to run in background", mare_shared::bindings::key_label(key))
        })
    }

    // --------------------------------------------------------------
    // frame
    // --------------------------------------------------------------
    /// One line about what the current frame contains (slow-frame ring, `/profile`).
    pub(in crate::app) fn frame_summary(&self) -> String {
        format!(
            "blocks {} (live {}), widgets {}, held commits {}, {}x{}{}",
            self.blocks.len(),
            self.blocks.len() - self.committed,
            self.widgets.len(),
            self.held.len(),
            self.width,
            self.height,
            if self.transcript.is_open() { ", transcript open" } else { "" }
        )
    }

    pub(in crate::app) fn render(&mut self) {
        self.keymap.sync_from(&self.ui.keymap.read().unwrap());
        let now = mare_tui::render::clock_now();
        self.maybe_show_delayed_prompts(now);
        if self.quit_armed.is_some_and(|t| self.modal.is_some() || now.saturating_duration_since(t) >= QUIT_WINDOW) {
            self.disarm_quit();
        }
        self.stream_commit_pending = false;
        if self.transcript.is_open() {
            return self.render_transcript();
        }
        let theme = self.theme();
        let w = self.width.max(10);
        let frame = self.anim_frame();

        // 0. captured external output: at most 8 lines a frame, above everything else
        let mut commit = self.take_external_output(&theme);

        // 1. commit finished leading blocks & streaming answer into scrollback
        let committed_before = self.committed;
        self.commit_finished_blocks(&mut commit, w, &theme, frame);
        let partial_before =
            (self.committed < self.blocks.len()).then(|| (self.committed, self.blocks[self.committed].partial));
        let mut stream_rows = self.commit_streaming_block(&mut commit, w, &theme, frame);
        let partial_held =
            partial_before.filter(|&(idx, before)| idx < self.blocks.len() && self.blocks[idx].partial > before);

        // 1c. `ui/commit-before` may rewrite or append to the lines (held until it answers;
        //     they stay visible in the live area meanwhile, and commits keep their order)
        let commit = self.route_commit(commit, committed_before, partial_held);

        // 2. stacked regions in configured order (`ui.layout`)
        // notifications: expire old cards (drawn later in overlay z-order)
        self.notices.prepare(now, self.modal.is_some());
        // the interrupted work has stopped: the next Ctrl+C is a normal one again
        if self.interrupted.is_some() && !self.stopping() {
            self.interrupted = None;
        }
        let max_h = self.height.saturating_sub(1).max(4);
        let order = self.layout.order.clone();
        let mut regions: Vec<RegionLines> = Vec::with_capacity(order.len());
        let mut live_stream_end = 0usize;
        for kind in order {
            let mut lines = Vec::new();
            let mut region_cursor = None;
            match kind {
                StackRegion::Live => {
                    live_stream_end = self.append_live_blocks(&mut lines, stream_rows.take(), w, &theme, frame);
                }
                StackRegion::Working => {
                    if !self.layout.is_hidden(StackRegion::Working) {
                        self.append_working_indicator(&mut lines, w, &theme, frame);
                    }
                }
                StackRegion::AboveEditor => {
                    if !self.layout.is_hidden(StackRegion::AboveEditor) {
                        for ((slot, _, _), item) in &self.widgets {
                            if slot == "above-editor" {
                                lines.extend(render_node(&item.node, w, &theme, frame));
                            }
                        }
                        if let Some(mh) = self.layout.max_height(StackRegion::AboveEditor) {
                            lines.truncate(mh);
                        }
                    }
                }
                StackRegion::BelowEditor => {
                    if !self.layout.is_hidden(StackRegion::BelowEditor) {
                        for ((slot, _, _), item) in &self.widgets {
                            if slot == "below-editor" {
                                lines.extend(render_node(&item.node, w, &theme, frame));
                            }
                        }
                        if let Some(mh) = self.layout.max_height(StackRegion::BelowEditor) {
                            lines.truncate(mh);
                        }
                    }
                }
                StackRegion::Completion => {
                    self.append_completion_popup(
                        &mut lines,
                        w,
                        &theme,
                        self.layout.max_height(StackRegion::Completion),
                    );
                }
                StackRegion::Editor => {
                    let (ed_lines, ed_cur) = self.append_editor(w, &theme, self.layout.max_height(StackRegion::Editor));
                    lines = ed_lines;
                    region_cursor = Some(ed_cur);
                }
                StackRegion::Status => {
                    lines.push(self.status_line(w, &theme, frame, false));
                }
            }
            regions.push(RegionLines { kind, lines, cursor: region_cursor });
        }

        // 3. height-insufficient clipping: `live` -> slots -> `working` -> `status` non-`keep`
        //    -> `editor` (keeping at least the cursor row)
        clip_regions(&mut regions, max_h, &mut live_stream_end, || self.status_line(w, &theme, frame, true));

        let mut live: Vec<Line> = vec![];
        let mut stream_end = 0usize;
        let mut working_row: Option<usize> = None;
        let mut editor_top = 0usize;
        let mut cursor: Option<(usize, usize)> = None;
        for r in regions {
            let start = live.len();
            match r.kind {
                StackRegion::Live => stream_end = start + live_stream_end.min(r.lines.len()),
                StackRegion::Working if !r.lines.is_empty() => working_row = Some(start),
                StackRegion::Editor => {
                    editor_top = start;
                    if self.modal.is_none()
                        && let Some((cr, cc)) = r.cursor
                    {
                        cursor = Some((start + cr, cc));
                    }
                }
                _ => {}
            }
            live.extend(r.lines);
        }

        // 4. overlays in fixed z-order: `notices` < `modal` < `which-key` (so select-modal which-key
        //    popups sit on top of the modal, and overlays do not change stacked region heights)
        let set = frame::BorderSet::for_theme(&theme.border, &theme.icons);
        let (boxed, pop_cursor, _pop_w) = self.modal_box(w, max_h, &theme, frame);
        let mut pop_top = None;
        let mut popup_anchor = editor_top;
        if !boxed.is_empty() {
            // Keep the 1-line working status row cleanly visible below the modal (above the editor)
            // whenever vertical room permits, instead of slicing it behind the modal borders.
            let below_working = working_row.filter(|&wr| wr < editor_top).map(|wr| live.len().saturating_sub(wr));
            if let (Some(wr), Some(below)) = (working_row, below_working)
                && boxed.len() + below <= max_h
            {
                popup_anchor = wr;
            }
            if popup_anchor < boxed.len() {
                let insert_at = popup_anchor;
                let extra = (boxed.len() - insert_at).min(max_h.saturating_sub(live.len()));
                for _ in 0..extra {
                    live.insert(insert_at, Line::empty());
                }
                popup_anchor += extra;
                if let Some(wr) = working_row.as_mut()
                    && *wr >= insert_at
                {
                    *wr += extra;
                }
                editor_top += extra;
                cursor = cursor.map(|(r, c)| (r + extra, c));
            }
            pop_top = Some(popup_anchor.saturating_sub(boxed.len()).min(live.len().saturating_sub(boxed.len())));
        }

        let layout = NoticeLayout { stream_end, pop_top, working_row, editor_top, max_h };
        self.flush_component_notices();
        let notice_pad = self.overlay_notices(&mut live, Some(layout), w, &theme, set, &mut cursor);
        editor_top += notice_pad;
        popup_anchor += notice_pad;

        if !boxed.is_empty() {
            overlay_popup(
                &mut live,
                &boxed,
                pop_cursor,
                &mut popup_anchor,
                &mut cursor,
                OverlayBounds { w, max_h, clear_row: true },
            );
        }
        if let Some((wk_box, _wk_w)) = self.which_key_box(w, &theme, set) {
            overlay_popup(
                &mut live,
                &wk_box,
                None,
                &mut editor_top,
                &mut cursor,
                OverlayBounds { w, max_h, clear_row: false },
            );
        }
        if live.len() > max_h {
            let cut = live.len() - max_h;
            live.drain(..cut);
            cursor = cursor.map(|(r, c)| (r.saturating_sub(cut), c));
        }
        // select mode: keep what's drawn (the text to copy) and where it sits on screen; the
        // selection shows in reverse video (frozen on the drag snapshot while a drag is active)
        let drawn = self.select.sel.map(|sel| {
            let drawn = live.clone();
            if let Some((snap, _, _)) =
                self.select.drag_snapshot.as_ref().filter(|(lines, _, snap_w)| *snap_w == w && lines.len() <= max_h)
            {
                live = snap.clone();
            }
            if let Some((a, b)) = sel.range() {
                highlight_selection(&mut live, a, b, w);
            }
            drawn
        });
        self.renderer.draw(&commit, &live, cursor);
        self.sync_position_known();
        self.maybe_request_cpr();
        if !boxed.is_empty() {
            self.mark_modal_drawn();
        }
        if let Some(drawn) = drawn {
            // the live area's top row: known from the startup probe; else it sits at the bottom
            // of a filled screen
            let top = self.renderer.top().unwrap_or_else(|| self.height.saturating_sub(live.len()) + 1);
            self.select.record(drawn, top, w);
        }
    }

    fn take_external_output(&mut self, theme: &Theme) -> Vec<Line> {
        let mut commit = vec![];
        let ext = mare_tui::capture::take_frame();
        if !ext.is_empty() {
            let base = theme.fg("dim");
            commit.extend(ext.iter().map(|t| mare_shared::ansi::sgr_line(t, base)));
            let plain: Vec<String> = ext.iter().map(|t| mare_shared::ansi::sanitize(t, false)).collect();
            self.emit("ui/external-output", serde_json::json!({ "lines": plain }));
        }
        commit
    }

    fn commit_finished_blocks(&mut self, commit: &mut Vec<Line>, w: usize, theme: &Theme, frame: usize) {
        let expand_key = self.editor_action_key("toggle-expand");
        while self.committed < self.blocks.len() && self.blocks[self.committed].finished {
            let idx = self.committed;
            let partial = self.blocks[idx].partial;
            // a picture still being prepared: the block stays live (rows reserved) until it's
            // ready, or until the wait limit turns it into text
            let (block, expand_all, show_reasoning) = (&mut self.blocks[idx], self.expand_all, self.show_reasoning);
            let (lines, settled) = self.image_hold.draw(block.id, mare_tui::render::clock_now(), || {
                block.render_tail(w, theme, frame, expand_all, show_reasoning, self.height, expand_key.as_deref())
            });
            if !settled {
                break;
            }
            if partial == 0 && (idx > 0 || !commit.is_empty()) {
                commit.push(Line::empty());
            }
            commit.extend(lines.into_iter().skip(partial));
            // drop streaming caches of committed blocks to save memory
            // scrollback holds the final text; keep only the source (Ctrl+T re-renders on demand)
            if let Some(md) = self.blocks[idx].md.as_mut() {
                md.drop_cache();
            }
            self.committed += 1;
        }
    }

    /// A streaming answer: its finished top-level blocks move into scrollback while it streams
    /// (the last block stays live — the next line may still change it); rows are paced by
    /// adaptive chunking: one per frame, or the whole backlog under a burst.
    /// Returns the streaming block's rows from its committed part on, reused for the live area.
    fn commit_streaming_block(
        &mut self,
        commit: &mut Vec<Line>,
        w: usize,
        theme: &Theme,
        frame: usize,
    ) -> Option<(usize, usize, Vec<Line>)> {
        if self.committed >= self.blocks.len() {
            return None;
        }
        let idx = self.committed;
        let now = mare_tui::render::clock_now();
        let show_reasoning = self.show_reasoning;
        let b = &mut self.blocks[idx];
        if b.finished || b.node.is_some() || b.expanded.is_some() || b.kind != "assistant" || b.md.is_none() {
            return None;
        }
        let skip = b.partial;
        let waited = mare_tui::image::waited();
        let lines = b.render_from(w, theme, frame, skip, show_reasoning);
        let (stable, src) = b.md.as_ref().map(|m| (m.stable_lines(), m.stable_src())).unwrap_or((0, 0));
        b.sc.sync_from(&lines, skip, stable, src, now);
        // nothing moves to the scrollback while a picture in it is being prepared
        let image_ready = mare_tui::image::waited() == waited;
        let rows = if image_ready { b.sc.tick(now) } else { vec![] };
        if !rows.is_empty() {
            if b.partial == 0 && (idx > 0 || !commit.is_empty()) {
                commit.push(Line::empty());
            }
            commit.extend(rows);
            b.partial = b.sc.drained;
        }
        // Self-drive smooth drain: if stable lines remain queued (not waiting on an image),
        // keep `dirty = true` so `sched.frame_delay` drains 1 line/frame even under
        // `reduce_motion`, unfocused, or standalone streaming blocks without `working`.
        self.stream_commit_pending = image_ready && b.sc.has_pending();
        Some((idx, skip, lines))
    }

    fn append_live_blocks(
        &mut self,
        live: &mut Vec<Line>,
        mut stream_rows: Option<(usize, usize, Vec<Line>)>,
        w: usize,
        theme: &Theme,
        frame: usize,
    ) -> usize {
        let expand_key = self.editor_action_key("toggle-expand");
        for h in &self.held {
            live.extend(h.lines.iter().cloned());
        }
        for i in self.committed..self.blocks.len() {
            let partial = self.blocks[i].partial;
            if i > 0 && partial == 0 {
                live.push(Line::empty());
            }
            match stream_rows.take_if(|(si, _, _)| *si == i) {
                Some((_, skip, rows)) => live.extend(rows.into_iter().skip(partial.saturating_sub(skip))),
                None => live.extend(
                    self.blocks[i]
                        .render_tail(
                            w,
                            theme,
                            frame,
                            self.expand_all,
                            self.show_reasoning,
                            self.height,
                            expand_key.as_deref(),
                        )
                        .into_iter()
                        .skip(partial),
                ),
            }
        }
        // the rows above hold the live stream (held commits, the answer and tools still running): a
        // card covering most of a row never overlays them
        let stream_end = live.len();
        if !live.is_empty() || self.committed > 0 {
            live.push(Line::empty());
        }
        stream_end
    }

    /// Working indicator: rendered through the single Lua component path `mare.ui.Working:render`
    /// , falling back to
    /// the generic `Fallback::Lines` text structure if Lua returns `nil` or errors.
    /// Returns its row so notification cards never cover it.
    fn append_working_indicator(&self, live: &mut Vec<Line>, w: usize, theme: &Theme, frame: usize) -> Option<usize> {
        let stopping = self.stopping();
        if !stopping && self.working.is_none() {
            return None;
        }
        let working_row = live.len();
        let now = mare_tui::render::clock_now();
        let (label, detail, secs, bg) = match &self.working {
            Some((l, d, started, background)) => (
                l.as_str(),
                d.as_deref(),
                now.saturating_duration_since(*started).as_secs(),
                self.background_hint(*background),
            ),
            None => ("Stopping", None, 0, None),
        };
        let still = frame == STATIC_FRAME;
        let interrupt_key = self.editor_action_key("interrupt");
        if let Some(n) =
            self.working_node(label, detail, secs, bg.as_slice(), interrupt_key.as_deref(), still, stopping, theme)
        {
            live.extend(render_node(&n, w, theme, frame).into_iter().take(1));
        } else {
            let text = if stopping {
                STOP_HINT.to_string()
            } else {
                let mut extra = vec![format!("{secs}s")];
                if let Some(d) = detail {
                    extra.push(d.to_string());
                }
                if let Some(key) = interrupt_key {
                    extra.push(format!("{key} to interrupt"));
                }
                if let Some(h) = bg {
                    extra.push(h);
                }
                format!("{label}… ({})", extra.join(" · "))
            };
            let fb = mare_tui::fallback::Fallback::lines(vec![Line::raw(text)]);
            live.extend(mare_tui::fallback::render(&fb, w, theme).into_iter().take(1));
        }
        Some(working_row)
    }

    /// Completion popup: Lua `mare.ui.CompletionPopup:render(p)` with generic `Fallback::Frame` + `Fallback::Lines`.
    fn append_completion_popup(&self, live: &mut Vec<Line>, w: usize, theme: &Theme, max_height: Option<usize>) {
        let Some(c) = &self.completion.popup else {
            *self.component_caches.completion_popup.borrow_mut() = None;
            return;
        };
        if c.items.is_empty() {
            return;
        }
        let cw = w.min(72);
        let cap = max_height.map(|mh| mh.saturating_sub(2).max(1)).unwrap_or(10);
        let height = c.items.len().min(10).min(cap);
        let commands = c.items.first().is_some_and(|i| i.insert.starts_with('/'));
        let command_argument = {
            let editor = self.ed();
            let text = editor.raw_text();
            text.starts_with('/') && text.contains(char::is_whitespace)
        };
        let files = c.items.first().is_some_and(|i| {
            i.insert.starts_with('@')
                || ["./", "../", "~/"].iter().any(|prefix| i.insert.starts_with(prefix))
                || (!command_argument && i.label.contains('/'))
        });
        let title = if commands {
            "Commands"
        } else if files {
            "Files"
        } else {
            "Completions"
        };
        let mut hints = Vec::new();
        let movement: Vec<_> =
            ["text.up", "text.down"].into_iter().filter_map(|action| self.editor_action_key(action)).collect();
        if !movement.is_empty() {
            hints.push(format!("{} move", movement.join("")));
        }
        for (action, label) in [("complete", "complete"), ("editor.submit", "run"), ("interrupt", "close")] {
            if let Some(key) = self.editor_action_key(action) {
                hints.push(format!("{key} {label}"));
            }
        }
        hints.push(format!("{}/{}", c.selected + 1, c.items.len()));
        let footer = hints.join(" · ");
        if let Some(boxed) = self.completion_popup_lines(
            title,
            commands,
            &c.items,
            c.selected,
            cw,
            height,
            max_height,
            &footer,
            theme,
            STATIC_FRAME,
        ) {
            live.extend(boxed);
            return;
        }
        let ascii = theme.icons == "ascii";
        let fb_lines: Vec<Line> = c
            .items
            .iter()
            .enumerate()
            .skip(c.selected.saturating_sub(height.saturating_sub(1)))
            .take(height)
            .map(|(i, it)| {
                let marker = if i == c.selected {
                    if ascii {
                        mare_tui::fallback::Fallback::CHOICE_MARKER_ASCII
                    } else {
                        mare_tui::fallback::Fallback::CHOICE_MARKER
                    }
                } else {
                    mare_tui::fallback::Fallback::CHOICE_PAD
                };
                let style = if i == c.selected { theme.style("MareSelection").bold().reverse() } else { Style::new() };
                let mut l = Line::new(vec![Span::styled(format!("{marker}{}", it.label), style)]);
                if let Some(d) = &it.detail {
                    l.push(Span::styled(format!("  {d}"), theme.fg("muted")));
                }
                l
            })
            .collect();
        let mut opts = frame::BoxOpts::for_theme(theme, theme.style("MareBorder"));
        opts.title = vec![Span::styled(title, theme.style("MareTitle"))];
        opts.footer = Some(footer);
        opts.footer_style = theme.style("MareDim");
        let mut boxed = frame::boxed(&fb_lines, cw, &opts);
        if let Some(mh) = max_height {
            boxed.truncate(mh);
        }
        live.extend(boxed);
    }

    /// Which-key popup: Lua `mare.ui.WhichKey:render(p)` with `Fallback::Frame` + `Fallback::Lines`.
    pub(in crate::app) fn which_key_box(
        &self,
        w: usize,
        theme: &Theme,
        _set: frame::BorderSet,
    ) -> Option<(Vec<Line>, usize)> {
        let Some((p, focus)) = self.pending_prefix.as_ref() else {
            *self.component_caches.which_key.borrow_mut() = None;
            return None;
        };
        let allow_with_modal = match &self.modal {
            None => true,
            Some(Modal::Select { kind, .. }) if kind != "confirm" && *focus == KeyFocus::Select => true,
            _ => false,
        };
        if !allow_with_modal {
            return None;
        }
        let ed_chain;
        let chain: Vec<&str> = match focus {
            KeyFocus::Editor => {
                ed_chain = self.current_editor_chain();
                ed_chain.iter().map(String::as_str).collect()
            }
            KeyFocus::Transcript => self.transcript_chain(),
            KeyFocus::Select => vec!["select"],
        };
        let prefix = format!("{p} ");
        let state = self.key_state();
        let (descriptions, per_binding) = self.binding_descriptions();
        let mut seen_wk = std::collections::HashSet::new();
        let mut next: Vec<(String, String)> = self
            .keymap
            .reachable_in_chain(&chain)
            .into_iter()
            .filter(|(_, _, e)| e.when.holds(state))
            .filter_map(|(ctx, k, e)| {
                k.strip_prefix(&prefix).filter(|_| seen_wk.insert(k.to_string())).map(|rest| {
                    let key =
                        rest.split_whitespace().map(mare_shared::bindings::key_label).collect::<Vec<_>>().join(" ");
                    let desc = per_binding
                        .get(&(ctx.to_string(), k.to_string(), e.action.clone()))
                        .cloned()
                        .unwrap_or_else(|| crate::app::input::action_description(&descriptions, &e.action));
                    (key, desc)
                })
            })
            .collect();
        let pretty_prefix = p.split_whitespace().map(mare_shared::bindings::key_label).collect::<Vec<_>>().join(" ");
        let cancel_action = match focus {
            KeyFocus::Editor => "interrupt",
            KeyFocus::Transcript => "view.back",
            KeyFocus::Select => "select.back",
        };
        let cancel = self
            .keymap
            .hint(cancel_action, &chain)
            .first()
            .map(|key| format!("{} cancel", mare_shared::bindings::key_label(key)));
        let pop_w = w.saturating_sub(4).clamp(20, 96).min(w.max(1));
        let key_w = next.iter().map(|(key, _)| mare_shared::style::str_width(key)).max().unwrap_or(0);
        let action_w = next.iter().map(|(_, action)| mare_shared::style::str_width(action)).max().unwrap_or(0);
        let cell = key_w + 2 + action_w;
        let columns = ((pop_w.saturating_sub(4) + 2) / (cell + 2)).max(1);
        next.truncate(self.height.saturating_sub(5).max(1).saturating_mul(columns));
        if let Some(res) = self.which_key_lines(&pretty_prefix, &next, cancel.as_deref(), pop_w, theme, STATIC_FRAME) {
            return Some(res);
        }
        let rows: Vec<Line> = next
            .chunks(columns)
            .map(|entries| {
                let mut row = Line::default();
                for (index, (key, action)) in entries.iter().enumerate() {
                    row.push(Span::styled(
                        format!("{key}{}", " ".repeat(key_w - mare_shared::style::str_width(key) + 2)),
                        theme.style("MareKeyHint"),
                    ));
                    row.push(Span::styled(action.clone(), theme.fg("MareMuted")));
                    if index + 1 < entries.len() {
                        row.push(Span::raw(" ".repeat(action_w - mare_shared::style::str_width(action) + 2)));
                    }
                }
                row
            })
            .collect();
        let mut opts = frame::BoxOpts::for_theme(theme, theme.style("MareBorderActive"));
        opts.title = vec![Span::styled(format!("+{pretty_prefix}"), theme.style("MareTitle"))];
        opts.footer = cancel;
        opts.footer_style = theme.style("MareDim");
        Some((frame::boxed(&rows, pop_w, &opts), pop_w))
    }

    fn append_editor(&self, w: usize, theme: &Theme, max_height: Option<usize>) -> (Vec<Line>, (usize, usize)) {
        let placeholder = if self.working.is_some() {
            self.editor_busy_placeholder.clone().unwrap_or_else(|| {
                let hints = [("steer", "steer now"), ("interrupt", "interrupt")]
                    .into_iter()
                    .filter_map(|(action, description)| {
                        self.editor_action_key(action).map(|key| format!("{key} {description}"))
                    })
                    .collect::<Vec<_>>();
                if hints.is_empty() {
                    "Queue a message…".into()
                } else {
                    format!("Queue a message… ({})", hints.join(" · "))
                }
            })
        } else {
            self.editor_placeholder.clone().unwrap_or_else(|| "Ask mare anything… (@ files, / commands, ? keys)".into())
        };
        // shell mode (look comes from the plugin that owns it): `!` on an empty input enters it
        let shell = self.ed().shell_level();
        let style = self.shell_style.as_ref().filter(|_| shell > 0);
        let placeholder = match style {
            None => placeholder,
            Some(s) if shell == 1 => s.placeholder.clone(),
            Some(s) => s.private_placeholder.clone(),
        };
        let prompt =
            if style.is_some() { format!("{} ", "!".repeat(shell as usize)) } else { self.editor_prompt.clone() };
        let border_color = match style {
            Some(s) => s.border.clone(),
            None => self.editor_border.clone(),
        };
        let chrome = self.editor_chrome_opts(
            &prompt,
            &placeholder,
            border_color.as_deref(),
            shell,
            self.working.is_some(),
            self.ed().is_empty(),
        );
        let content_max = match max_height {
            Some(mh) if mh >= 3 => mh - 2,
            Some(_) => 1,
            None => (self.height / 3).max(3),
        };
        let (prompt, placeholder, mut border_color, mut prompt_style, placeholder_style) = match chrome {
            Some(c) => (c.prompt, c.placeholder, c.border, c.prompt_style, c.placeholder_style),
            None => (prompt, placeholder, border_color, None, None),
        };
        if !self.ui.focused.load(std::sync::atomic::Ordering::Relaxed) {
            border_color = Some("MareDim".into());
            prompt_style = Some("MareDim".into());
        }
        let opts = EditorRenderOpts {
            prompt,
            placeholder,
            hint: self.completion.hint.as_ref().map(|(_, hint)| hint.clone()),
            max_height: content_max,
            border_color,
            prompt_style,
            placeholder_style,
            ..Default::default()
        };
        let mut r = self.ed().render(w, theme, &opts);
        let mut cr = r.cursor.0 as usize;
        let cc = r.cursor.1 as usize;
        if let Some(mh) = max_height
            && mh < 3
            && r.lines.len() > mh
        {
            let keep = mh.max(1);
            let start = cr.saturating_sub(keep - 1).min(r.lines.len() - keep);
            r.lines = r.lines.drain(start..start + keep).collect();
            cr -= start;
        }
        (r.lines, (cr, cc))
    }

    /// Notification cards at the right of the live area, newest at the top (Yazi notify). With
    /// a popup open a card never covers it and never grows the live area; deferred cards wait
    /// without expiring until the popup closes.
    /// Notification cards at the right of the live area (or transcript when `layout` is `None`),
    /// newest at the top (Yazi notify). Deferred cards wait without expiring until room returns.
    pub(super) fn overlay_notices(
        &mut self,
        live: &mut Vec<Line>,
        layout: Option<NoticeLayout>,
        w: usize,
        theme: &Theme,
        _set: frame::BorderSet,
        cursor: &mut Option<(usize, usize)>,
    ) -> usize {
        if self.notices.is_empty() {
            return 0;
        }
        if layout.is_none() && (self.modal.is_some() || live.len() <= 2) {
            self.notices.defer(mare_tui::render::clock_now());
            return 0;
        }
        let max_above = layout.as_ref().map(|l| {
            let room = l.pop_top.unwrap_or(l.editor_top).min(l.working_row.unwrap_or(usize::MAX));
            room + l.max_h.saturating_sub(live.len())
        });
        let budget = match &layout {
            Some(l) => l.pop_top.map(|top| top.saturating_sub(live.len().saturating_sub(l.max_h))),
            None => Some(live.len() - 2),
        };
        let half_cap = layout.as_ref().map(|l| l.max_h / 2);
        let covering_budget =
            layout.as_ref().and_then(|l| (l.stream_end > 0).then(|| l.max_h.saturating_sub(live.len())));
        let mut covering = false;
        let mut visible_cards = 0;
        let cw = (w / 2).clamp(24, 44).min(w).max(6);
        let mut cards: Vec<(usize, Line)> = vec![];
        for (text, level, _) in self.notices.iter().rev() {
            let is_first = self.notices.is_first_card(text);
            let (style, name, icon_key) = alert_style_keys(level);
            let icon = icon_for(theme, icon_key);
            let (card_w, mut max_rows) = box_fit_metrics(text, is_first, w, cw);
            // When space above the editor/working line is tight (e.g. narrow 24x10 terminals),
            // clip the newest card's body rows to fit the available height instead of suppressing it.
            if layout.is_some() && cards.is_empty() {
                let covers_est = w.saturating_sub(card_w) < w / 2;
                let mut cap = usize::MAX;
                if let Some(b) = budget {
                    cap = cap.min(b);
                }
                if let Some(b) = max_above {
                    cap = cap.min(b);
                }
                if covers_est && let Some(b) = covering_budget {
                    cap = cap.min(b);
                }
                if cap >= 3 {
                    max_rows = max_rows.min(cap - 2);
                }
            }
            let mut card = self
                .notice_card_lines(text, level, icon, name, style, is_first, card_w, max_rows, theme, STATIC_FRAME)
                .unwrap_or_else(|| build_notice_card(text, theme, (style, name, icon), (card_w, max_rows)));
            let max_w = card_w.min(w);
            for line in &mut card {
                if line.width() > max_w {
                    *line = truncate_line(line, max_w);
                }
            }
            let actual_w = card.iter().map(Line::width).max().unwrap_or(0).min(w);
            let x = w.saturating_sub(actual_w);
            let covers = x < w / 2;
            let next_len = cards.len() + card.len();
            if budget.is_some_and(|b| next_len > b)
                || max_above.is_some_and(|b| next_len > b)
                || (!cards.is_empty() && half_cap.is_some_and(|b| next_len > b))
                || covers && covering_budget.is_some_and(|b| next_len > b)
            {
                break;
            }
            visible_cards += 1;
            covering |= covers;
            cards.extend(card.into_iter().map(|l| (x, l)));
        }
        let mut extra = 0;
        let start_row = match layout {
            Some(layout) => {
                let room = if covering && layout.stream_end > 0 {
                    0
                } else {
                    layout.pop_top.unwrap_or(layout.editor_top).min(layout.working_row.unwrap_or(usize::MAX))
                };
                if room < cards.len() {
                    extra = (cards.len() - room).min(layout.max_h.saturating_sub(live.len()));
                    for _ in 0..extra {
                        live.insert(0, Line::empty());
                    }
                    *cursor = cursor.map(|(r, c)| (r + extra, c));
                }
                0
            }
            None => 1,
        };
        for (i, (x, c)) in cards.iter().take(live.len().saturating_sub(start_row)).enumerate() {
            let row = start_row + i;
            if live[row].width() > w {
                live[row] = slice_cells(&live[row], 0, w);
            }
            live[row] = overlay_line(&live[row], *x, c);
        }
        self.notices
            .set_visible_count(if self.modal.is_some() { 0 } else { visible_cards }, mare_tui::render::clock_now());
        extra
    }

    pub(super) fn status_line(&self, w: usize, theme: &Theme, frame: usize, drop_non_keep: bool) -> Line {
        let ascii = theme.icons == "ascii";
        let queued = self.ui.queued.load(std::sync::atomic::Ordering::Relaxed);
        let queue_mode = self.ui.queue_mode.load(std::sync::atomic::Ordering::Relaxed);
        if queued > 0 && queue_mode == mare_tui::queue_keys::SELECTING {
            let entries: Vec<(String, String)> = [
                ("text.up", "previous"),
                ("text.down", "next"),
                ("editor.submit", "edit"),
                ("text.delete_forward", "remove"),
                ("interrupt", "back"),
            ]
            .into_iter()
            .filter_map(|(action, label)| self.editor_action_key(action).map(|key| (key, label.into())))
            .collect();
            if !entries.is_empty() {
                if let Some(line) = self.hints_line(&entries, theme) {
                    return truncate_line(&line, w);
                }
                let mut line = Line::empty();
                for (key, label) in entries {
                    if !line.spans.is_empty() {
                        line.push(Span::styled(if ascii { " - " } else { " · " }, theme.fg("MareDim")));
                    }
                    line.push(Span::styled(format!("{key} {label}"), theme.fg("MareDim")));
                }
                return truncate_line(&line, w);
            }
        }
        // an overlay is open: the status bar becomes its key hints (lazygit options bar)
        let mut hints: Vec<(String, String)> =
            self.modal_hints().into_iter().map(|(k, d)| (k, d.to_string())).collect();
        // a select's extra action keys (`mare.ui.select{keys=…}`), before "cancel"
        if let Some(Modal::Select { keys, searching: false, .. }) = &self.modal {
            let at = hints.iter().position(|(_, d)| d == "cancel").unwrap_or(hints.len());
            for (i, (c, label)) in keys.iter().enumerate() {
                hints.insert(at + i, (key_name(*c), if label.is_empty() { "action".into() } else { label.clone() }));
            }
        }
        if !hints.is_empty() {
            let render_hints = |entries: &[(String, String)]| {
                if let Some(line) = self.hints_line(entries, theme) {
                    return line;
                }
                let mut line = Line::new(vec![Span::raw(" ")]);
                for (i, (key, desc)) in entries.iter().enumerate() {
                    if i > 0 {
                        line.push(Span::styled(if ascii { " | " } else { " │ " }, theme.fg("MareSubtle")));
                    }
                    line.push(Span::styled(key.clone(), theme.style("MareKeyHint")));
                    line.push(Span::styled(format!(" {desc}"), theme.fg("MareDim")));
                }
                line
            };
            // Drop complete hints, preserving dismissal and interruption before optional navigation.
            let rank = |desc: &str| match desc {
                "all keys" => 9,
                "preview" | "toggle preview" | "scroll" => 8,
                "search" | "switch tab" => 7,
                "move" => 6,
                "this session" | "remember" => 5,
                "toggle" => 4,
                "cancel" | "clear search" | "interrupt" => 0,
                _ => 2,
            };
            let line = render_hints(&hints);
            if line.width() <= w {
                return line;
            }
            let ellipsis = if ascii { "..." } else { "…" };
            let ew = mare_shared::style::str_width(ellipsis);
            while !hints.is_empty() {
                let remove = hints.iter().enumerate().max_by_key(|(_, (_, desc))| rank(desc)).map(|(i, _)| i).unwrap();
                hints.remove(remove);
                let mut line = render_hints(&hints);
                if line.width() + 1 + ew <= w {
                    line.push(Span::styled(format!(" {ellipsis}"), theme.fg("MareSubtle")));
                    return line;
                }
                let mut compact = Line::empty();
                for (i, (key, desc)) in hints.iter().enumerate() {
                    if i > 0 {
                        compact.push(Span::raw(" "));
                    }
                    compact.push(Span::styled(key.clone(), theme.style("MareKeyHint")));
                    compact.push(Span::styled(format!(" {desc}"), theme.fg("MareDim")));
                }
                if compact.width() + ew <= w {
                    compact.push(Span::styled(ellipsis, theme.fg("MareSubtle")));
                    return compact;
                }
            }
            return if w >= ew { Line::raw(ellipsis) } else { Line::empty() };
        }
        let sep = Span::styled(if ascii { " - " } else { " · " }, theme.fg("subtle"));
        let mut left: Vec<Span> = vec![Span::raw("  ")];
        let mut right: Vec<Vec<Span>> = vec![];
        // the left segments flagged `keep` (the permission-mode badge): whole when narrow
        let mut badge_end = 0;
        for ((align, _, _, _), seg) in &self.status {
            let (n, keep) = (&seg.node, seg.keep);
            if drop_non_keep && !keep {
                continue;
            }
            let spans: Vec<Span> = match n {
                Node::Line(line) if align == "right" => line.resolve(theme).spans,
                _ => render_node(n, w, theme, frame).into_iter().next().map(|l| l.spans).unwrap_or_default(),
            };
            if spans.is_empty() {
                continue;
            }
            if align == "right" {
                let mut group = Vec::new();
                for span in spans {
                    if span.text == " · " || span.text == " - " {
                        if !group.is_empty() {
                            right.push(std::mem::take(&mut group));
                        }
                    } else {
                        group.push(span);
                    }
                }
                if !group.is_empty() {
                    right.push(group);
                }
            } else {
                if left.len() > 1 {
                    left.push(sep.clone());
                }
                left.extend(spans);
            }
            if align != "right" && keep {
                badge_end = left.len();
            }
        }
        if !drop_non_keep && self.startup.error.is_some() {
            left.push(sep.clone());
            left.push(Span::styled("Startup blocked", theme.fg("dim")));
        }
        if !drop_non_keep && let Some(h) = &self.hint {
            left.push(sep.clone());
            left.push(Span::styled(h.clone(), theme.fg("dim")));
        }
        if !drop_non_keep
            && queued > 0
            && queue_mode == mare_tui::queue_keys::OFF
            && self.ed().is_empty()
            && !self.history.browsing()
            && !self.completion.is_open()
        {
            for (action, label) in [("text.up", "to edit queued"), ("editor.submit", "queue options")] {
                if let Some(key) = self.editor_action_key(action) {
                    left.push(sep.clone());
                    left.push(Span::styled(format!("{key} {label}"), theme.fg("MareDim")));
                }
            }
        }
        let lw: usize = left.iter().map(Span::width).sum();
        let right_width = |groups: &[Vec<Span>]| {
            groups.iter().flatten().map(Span::width).sum::<usize>() + groups.len().saturating_sub(1) * sep.width()
        };
        let rw = right_width(&right);
        let join_right = |groups: Vec<Vec<Span>>| {
            let mut spans = Vec::new();
            for group in groups {
                if !spans.is_empty() {
                    spans.push(sep.clone());
                }
                spans.extend(group);
            }
            spans
        };
        if lw + rw + 2 < w {
            let mut l = Line::new(left);
            l.push(Span::raw(" ".repeat(w - lw - rw - 1)));
            for s in join_right(right) {
                l.push(s);
            }
            return truncate_line(&l, w);
        }
        // too narrow: the mode badge stays whole,
        // the rest of the left side gives way first; right-side segments stay whole
        let rest = left.split_off(badge_end.min(left.len()));
        let bw: usize = left.iter().map(Span::width).sum();
        while !right.is_empty() && bw + right_width(&right) + 1 > w {
            right.remove(0);
        }
        let rw = right_width(&right);
        let mut l = Line::new(left);
        let rest_room = w.saturating_sub(bw + rw + 1);
        if rest_room >= 6 {
            l.spans.extend(truncate_line(&Line::new(rest), rest_room).spans);
        }
        let used = l.width();
        let room = w.saturating_sub(used + 1);
        if room >= 4 && !right.is_empty() {
            let r = Line::new(join_right(right));
            l.push(Span::raw(" ".repeat(w - used - r.width())));
            l.spans.extend(r.spans);
        }
        truncate_line(&l, w)
    }
}

/// How a select action key is shown, including control characters as `Ctrl+D`.
fn key_name(c: char) -> String {
    match c {
        '\t' => mare_shared::bindings::key_label("tab"),
        c if (c as u32) < 0x20 => mare_shared::bindings::key_label(&format!("ctrl+{}", ((c as u8) | 0x60) as char)),
        c => c.to_string(),
    }
}

pub(super) struct NoticeLayout {
    stream_end: usize,
    pop_top: Option<usize>,
    working_row: Option<usize>,
    editor_top: usize,
    max_h: usize,
}

struct OverlayBounds {
    w: usize,
    max_h: usize,
    clear_row: bool,
}

fn overlay_popup(
    live: &mut Vec<Line>,
    boxed: &[Line],
    pop_cursor: Option<(usize, usize)>,
    editor_top: &mut usize,
    cursor: &mut Option<(usize, usize)>,
    bounds: OverlayBounds,
) -> Option<usize> {
    if boxed.is_empty() {
        return None;
    }
    let OverlayBounds { w, max_h, clear_row } = bounds;
    let bw = boxed.iter().map(Line::width).max().unwrap_or(0);
    // Reserve space above the editor when the terminal has room (`<= max_h`), so modals sit
    // above the editor without covering it; on tiny terminals, float without exceeding `max_h`.
    if *editor_top < boxed.len() {
        let needed = boxed.len() - *editor_top;
        let extra = needed.min(max_h.saturating_sub(live.len()));
        for _ in 0..extra {
            live.insert(*editor_top, Line::empty());
        }
        *editor_top += extra;
        *cursor = cursor.map(|(r, c)| (r + extra, c));
    }
    let top = editor_top.saturating_sub(boxed.len()).min(live.len().saturating_sub(boxed.len()));
    let x = (w.saturating_sub(bw)) / 2;
    let empty = Line::empty();
    for (i, b) in boxed.iter().take(live.len().saturating_sub(top)).enumerate() {
        let base = if clear_row { &empty } else { &live[top + i] };
        live[top + i] = overlay_line(base, x, b);
    }
    if let Some((cr, cc)) = pop_cursor {
        *cursor = Some((top + 1 + cr, x + 2 + cc));
    }
    Some(top)
}

fn alert_style_keys(level: &str) -> (&'static str, &'static str, &'static str) {
    match level {
        "error" => ("MareNotifyError", "Error", "error"),
        "warning" | "warn" => ("MareNotifyWarn", "Warning", "warn"),
        _ => ("MareNotifyInfo", if level == "success" { "Done" } else { "Info" }, "info"),
    }
}

fn box_fit_metrics(text: &str, is_first: bool, w: usize, cw: usize) -> (usize, usize) {
    let cw = cw.max(6);
    if !(is_first && text.contains('\n')) {
        return (cw, 5);
    }
    let body_at = |cw: usize| {
        let inner = cw.saturating_sub(4).max(1);
        let mut body = vec![];
        for part in text.lines().take(4) {
            body.extend(wrap_line(&Line::raw(expand_tabs(part)), inner));
        }
        body
    };
    let first = text.lines().next().unwrap_or_default();
    let max_rows = wrap_line(&Line::raw(expand_tabs(first)), cw.saturating_sub(4).max(1)).len().clamp(1, 5);
    let mut card_w = cw;
    let mut body = body_at(cw);
    while body.len() > max_rows && card_w < w {
        card_w = (card_w + 4).min(w);
        body = body_at(card_w);
    }
    (card_w, max_rows)
}

fn build_notice_card(
    text: &str,
    theme: &Theme,
    (style, name, icon): (&str, &str, &str),
    (card_w, max_rows): (usize, usize),
) -> Vec<Line> {
    let set = frame::BorderSet::for_theme(&theme.border, &theme.icons);
    let vw = str_width(set.v);
    let inner = card_w.saturating_sub(vw * 2 + 2).max(1);
    let clean = sanitize(text);
    let mut body = vec![];
    for part in clean.split('\n') {
        body.extend(wrap_line(&Line::raw(expand_tabs(part)), inner));
    }
    if body.len() > max_rows {
        let shown = max_rows.saturating_sub(1);
        let omitted = body.len() - shown;
        body.truncate(shown);
        body.push(Line::styled(format!("↓ {omitted} more lines"), theme.fg("MareDim")));
    }
    let fb = mare_tui::fallback::Fallback::frame(
        vec![Span::styled(format!("{icon} {name}"), theme.style(style))],
        style,
        vec![mare_tui::fallback::Fallback::lines(body)],
    );
    mare_tui::fallback::render(&fb, card_w, theme)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_notice_reports_the_number_of_clipped_lines() {
        let text = (1..=8).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let lines = build_notice_card(&text, &Theme::default(), ("MareNotifyInfo", "Info", "i"), (40, 3));
        let rendered = lines.iter().map(Line::plain).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("line 1") && rendered.contains("line 2"), "{rendered}");
        assert!(rendered.contains("↓ 6 more lines"), "{rendered}");
    }

    #[test]
    fn fallback_notice_keeps_every_row_within_box_borders() {
        let theme = Theme::default();
        let samples = [
            "Retrying (1) in 0.5s: stream ended before response.completed",
            "https://api.example.com/v1/responses/very-long-unbreakable-path-segment-that-exceeds-card-width",
            "\x1b[31mred\x1b[0m\t中文宽字符测试超长提示信息不断行边界检查",
            "short",
        ];
        for w in [16usize, 44, 110] {
            let cw = (w / 2).clamp(24, 44).min(w);
            for text in samples {
                let (card_w, max_rows) = box_fit_metrics(text, false, w, cw);
                let lines = build_notice_card(text, &theme, ("MareNotifyWarn", "Warning", "⚠"), (card_w, max_rows));
                assert!(lines.len() >= 3, "box needs top/body/bottom for {text:?} at w={w}");
                for (idx, line) in lines.iter().enumerate() {
                    let plain = line.plain();
                    assert_eq!(
                        line.width(),
                        card_w,
                        "row {idx} width {} != card_w {card_w} (w={w}) for {text:?}:\n{plain}",
                        line.width()
                    );
                    if idx > 0 && idx + 1 < lines.len() {
                        assert!(
                            plain.starts_with('│') && plain.ends_with('│'),
                            "body row {idx} lost borders for {text:?}:\n{plain}"
                        );
                    }
                }
            }
        }
    }
}
