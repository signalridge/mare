//! Modal overlays: their types, highlight reports, status-bar hints, the drawn panel.

use crate::app::*;

pub(in crate::app) enum Modal {
    Select {
        title: String,
        /// at the right of the top border (`mare.ui.select{title_right=…}`)
        title_right: Vec<Span>,
        /// the list's share of a wide popup next to a preview (`mare.ui.select{split=…}`)
        split: mare_shared::ui::SelectSplit,
        items: Vec<SelectItem>,
        filter: String,
        selected: usize,
        multi: bool,
        accept_label: Option<String>,
        allow_search: bool,
        /// tabbed views; `items`/`selected` hold the active tab's
        tabs: Vec<SelectTab>,
        tab: usize,
        /// "list" | "confirm"
        kind: String,
        /// extra action keys (`mare.ui.select{keys=…}`)
        keys: Vec<(char, String)>,
        /// explanatory lines above the options (`mare.ui.select{body=…}`)
        body: Vec<String>,
        /// scroll offset for confirm message or `body` lines when they exceed available height
        body_scroll: usize,
        /// last measured max scroll offset for confirm message or `body` lines
        body_scroll_max: Option<usize>,
        /// scroll offset for the preview pane when it exceeds available height
        preview_scroll: usize,
        /// last measured max scroll offset for the preview pane
        preview_scroll_max: Option<usize>,
        preview_hidden: bool,
        /// `/` pressed (or typing): the border turns to the search color
        searching: bool,
        /// vim-style keys while not searching (`mare.ui.select{vim=true}`)
        vim: bool,
        /// pending count prefix for j/k (vim)
        count: usize,
        /// told the highlighted (tab, item, list_rev) when it changes (lazily loaded previews)
        highlight: Option<tokio::sync::mpsc::UnboundedSender<(usize, usize, u64)>>,
        last_hl: Option<(usize, usize)>,
        reply: Option<oneshot::Sender<Result<SelectReply, Dismissed>>>,
        /// names it for `UiCmd::SelectUpdate` (0: none)
        token: u64,
        /// unique modal id for `UiCmd::CancelModal` / `UiCmd::SelectPreview` (0: none)
        modal_id: u64,
        /// incremented on every `SelectUpdate` so stale `SelectPreview` replies are ignored
        list_rev: u64,
        filter_cache: SelectFilterCache,
        /// Tool-approval modal (`mare.ui.approve`).
        approval: bool,
        /// Set to `true` once this modal has been drawn at least once on screen.
        drawn: bool,
    },
    /// `?`: every key of the current context, searchable; Enter runs it (lazygit keybindings menu)
    Help { items: Vec<SelectItem>, acts: Vec<HelpAct>, filter: String, selected: usize },
    Input {
        title: String,
        editor: Editor,
        secret: bool,
        placeholder: String,
        reply: Option<oneshot::Sender<Result<String, Dismissed>>>,
        modal_id: u64,
    },
    Info {
        title: String,
        node: Node,
        scroll: usize,
        scroll_max: Option<usize>,
        context: Option<String>,
        cursor: Option<(usize, usize)>,
    },
}

impl Modal {
    /// Whether the active modal displays any animated node (e.g. an item preview).
    pub(in crate::app) fn has_animation(&self) -> bool {
        match self {
            Self::Select { items, filter, selected, tab, filter_cache, .. } => filter_cache
                .get(items, *tab, filter)
                .get(*selected)
                .and_then(|&i| items[i].preview.as_ref())
                .is_some_and(Node::has_animation),
            Self::Info { node, .. } => node.has_animation(),
            _ => false,
        }
    }

    pub(in crate::app) fn modal_id(&self) -> Option<u64> {
        match self {
            Self::Select { modal_id, .. } | Self::Input { modal_id, .. } if *modal_id != 0 => Some(*modal_id),
            _ => None,
        }
    }
}

/// What a `?` help entry does when chosen.
pub(in crate::app) enum HelpAct {
    /// Non-selectable section heading.
    Group,
    /// replay a key in the context the help was opened from
    Key(KeyEvent),
    /// run a keymap action
    Action(String),
}

impl App {
    /// Tell a select's owner which item is highlighted now, when that changed (lazily loaded
    /// preview panes: `mare.ui.select{on_highlight=…}`).
    pub(in crate::app) fn notify_highlight(&mut self) {
        if let Some(Modal::Select {
            items,
            filter,
            selected,
            tab,
            highlight: Some(h),
            last_hl,
            list_rev,
            filter_cache,
            ..
        }) = self.modal.as_mut()
            && let Some(&i) = filter_cache.get(items, *tab, filter).get(*selected)
            && items[i].is_selectable()
            && *last_hl != Some((*tab, i))
        {
            *last_hl = Some((*tab, i));
            let _ = h.send((*tab, i, *list_rev));
        }
    }

    /// Keymap context chain for a `Modal::Info` overlay, falling back to `["info"]` if a custom
    /// plugin subcontext was removed while the overlay remained open.
    pub(in crate::app) fn info_context_chain(&self, context: Option<&str>) -> Vec<String> {
        context
            .and_then(|c| self.keymap.chain_for(c).ok())
            .map(|v| v.into_iter().map(str::to_owned).collect())
            .unwrap_or_else(|| vec!["info".to_string()])
    }

    fn modal_hint_entry<'a>(&self, actions: &[&str], chain: &[&str], label: &'a str) -> Option<(String, &'a str)> {
        let keys: Vec<String> = actions
            .iter()
            .filter_map(|a| self.keymap.hint(a, chain).first().map(|k| mare_shared::bindings::key_label(k)))
            .collect();
        if keys.is_empty() {
            return None;
        }
        let sep = if keys.iter().all(|s| s.chars().count() == 1) { "" } else { "/" };
        Some((keys.join(sep), label))
    }

    /// Key hints for the status bar while an overlay is open (lazygit options bar).
    pub(super) fn modal_hints(&self) -> Vec<(String, &str)> {
        let mut h = Vec::new();
        fn push<'a>(out: &mut Vec<(String, &'a str)>, item: Option<(String, &'a str)>) {
            if let Some(entry) = item {
                out.push(entry);
            }
        }
        match &self.modal {
            Some(Modal::Select { kind, accept_label, .. }) if kind == "confirm" => {
                let c = &["confirm"];
                push(&mut h, self.modal_hint_entry(&["confirm.yes"], c, "yes"));
                push(&mut h, self.modal_hint_entry(&["confirm.no"], c, "no"));
                push(&mut h, self.modal_hint_entry(&["confirm.toggle"], c, "choose"));
                let accept = accept_label.as_deref().unwrap_or("confirm");
                if !accept.is_empty() {
                    push(&mut h, self.modal_hint_entry(&["confirm.accept"], c, accept));
                }
                push(&mut h, self.modal_hint_entry(&["confirm.cancel"], c, "cancel"));
            }
            Some(Modal::Select {
                tabs,
                searching,
                vim,
                multi,
                allow_search,
                accept_label,
                items,
                filter,
                selected,
                tab,
                body_scroll_max,
                filter_cache,
                ..
            }) => {
                let c = crate::app::input::select_chain(*searching, *vim);
                let accept = accept_label.as_deref().unwrap_or(if *multi { "apply" } else { "select" });
                if !accept.is_empty() {
                    push(&mut h, self.modal_hint_entry(&["select.accept"], c, accept));
                }
                if *multi && !*searching {
                    push(&mut h, self.modal_hint_entry(&["select.toggle"], c, "toggle"));
                }
                let mut movement = self.modal_hint_entry(&["select.up", "select.down"], c, "move");
                if *vim
                    && !*searching
                    && self.keymap.hint("select.up", c).contains(&"k")
                    && self.keymap.hint("select.down", c).contains(&"j")
                    && let Some((keys, _)) = &mut movement
                {
                    keys.push_str("/jk");
                }
                push(&mut h, movement);
                if *searching {
                    push(&mut h, self.modal_hint_entry(&["select.back"], c, "clear search"));
                } else if *allow_search {
                    push(&mut h, self.modal_hint_entry(&["select.search"], c, "search"));
                }
                if filter_cache.get(items, *tab, filter).get(*selected).is_some_and(|&i| items[i].preview.is_some()) {
                    push(&mut h, self.modal_hint_entry(&["select.preview_up", "select.preview_down"], c, "preview"));
                    push(&mut h, self.modal_hint_entry(&["select.toggle_preview"], c, "toggle preview"));
                }
                if body_scroll_max.is_some_and(|m| m > 0) {
                    push(&mut h, self.modal_hint_entry(&["select.body_up", "select.body_down"], c, "scroll"));
                }
                if tabs.len() > 1 {
                    push(&mut h, self.modal_hint_entry(&["select.tab_next"], c, "switch tab"));
                }
                if !*searching {
                    push(&mut h, self.modal_hint_entry(&["select.back"], c, "cancel"));
                }
                push(&mut h, self.modal_hint_entry(&["select.help"], c, "all keys"));
            }
            Some(Modal::Help { .. }) => {
                let c = &["help"];
                push(&mut h, self.modal_hint_entry(&["help.accept"], c, "run"));
                h.push(("type".to_string(), "search"));
                push(&mut h, self.modal_hint_entry(&["help.cancel"], c, "back"));
            }
            Some(Modal::Input { .. }) => {
                let chain = self.keymap.chain_for("input").unwrap_or_else(|_| vec!["input", "text"]);
                for (act, label) in [("input.accept", "confirm"), ("input.cancel", "cancel")] {
                    if let Some(k) = self.keymap.hint(act, &chain).first() {
                        h.push((mare_shared::bindings::key_label(k), label));
                    }
                }
            }
            Some(Modal::Info { context, scroll_max, .. }) => {
                let owned = self.info_context_chain(context.as_deref());
                let chain: Vec<&str> = owned.iter().map(String::as_str).collect();
                if scroll_max.is_some_and(|m| m > 0) {
                    push(&mut h, self.modal_hint_entry(&["info.up", "info.down"], &chain, "scroll"));
                }
                if context.is_some() {
                    push(&mut h, self.modal_hint_entry(&["info.close"], &chain, "close"));
                } else {
                    h.push(("any key".to_string(), "close"));
                }
            }
            None if self.select.active() => {
                h.push(("drag".to_string(), "select text"));
                h.push(("release".to_string(), "copy"));
                let owned = self.current_editor_chain();
                let chain: Vec<&str> = owned.iter().map(String::as_str).collect();
                push(&mut h, self.modal_hint_entry(&["interrupt"], &chain, "cancel"));
            }
            None => {}
        }
        h
    }

    /// The open modal as a rounded panel (empty when none), the cursor inside it (an input), and
    /// its width. Drawn over whatever is on screen: the live area above the editor, or the
    /// transcript overlay.
    pub(super) fn modal_box(
        &mut self,
        w: usize,
        max_h: usize,
        theme: &Theme,
        frame: usize,
    ) -> (Vec<Line>, Option<(usize, usize)>, usize) {
        if self.modal.is_none() {
            self.component_caches.select_row.borrow_mut().clear();
            *self.component_caches.modal_frame.borrow_mut() = None;
            return (Vec::new(), None, 0);
        }
        let mut boxed: Vec<Line> = vec![];
        let mut pop_cursor: Option<(usize, usize)> = None;
        let mut pop_w = w.saturating_sub(4).clamp(20, 96).min(w.max(1));
        let default_list_h = (self.height / 2).clamp(1, 15);
        let render_list = |items: &[SelectItem],
                           visible: &[usize],
                           filter: &str,
                           selected: usize,
                           inner: usize,
                           cap_h: usize,
                           multi: bool|
         -> (Vec<Line>, frame::Scroll, usize) {
            let n = visible.len();
            let hl = theme.style("MareAccent").bold();
            let build_item = |vis_idx: usize, i: usize, use_lua: bool| -> (Vec<Line>, bool) {
                let it = &items[i];
                let checkbox = multi && it.is_selectable();
                let content_width = inner.saturating_sub(if checkbox { 4 } else { 0 });
                let mut from_lua = false;
                let mut lines = if !it.lines.is_empty() {
                    it.lines.iter().map(|l| frame::highlight_line(&l.resolve(theme), filter, hl)).collect()
                } else if use_lua
                    && let Some(lines) =
                        self.select_row_lines(it, i, vis_idx == selected, filter, content_width, theme, frame)
                {
                    from_lua = true;
                    lines
                } else {
                    let mut raw = it.label.clone();
                    if let Some(d) = &it.detail {
                        raw.push_str("  ");
                        raw.push_str(d);
                    }
                    mare_tui::fallback::render(
                        &mare_tui::fallback::Fallback::lines(vec![Line::raw(raw)]),
                        content_width,
                        theme,
                    )
                };
                if checkbox {
                    for (row, line) in lines.iter_mut().enumerate() {
                        let prefix = if row == 0 { if it.checked { "[x] " } else { "[ ] " } } else { "    " };
                        line.spans.insert(0, Span::styled(prefix, theme.style("MareDim")));
                    }
                }
                (lines, from_lua)
            };
            let (is_preset, has_lua_row) = self.sync_select_row_state();
            if n > 0 {
                let mut starts = Vec::with_capacity(n + 1);
                let mut total = 0usize;
                for (vis_idx, &i) in visible.iter().enumerate() {
                    starts.push(total);
                    let it = &items[i];
                    let h = if !it.lines.is_empty() {
                        it.lines.len().max(1)
                    } else if has_lua_row && is_preset {
                        1
                    } else if !has_lua_row {
                        let checkbox = multi && it.is_selectable();
                        let content_width = inner.saturating_sub(if checkbox { 4 } else { 0 }).max(1);
                        let raw_bytes = it.label.len() + it.detail.as_ref().map_or(0, |d| 2 + d.len());
                        if raw_bytes <= content_width
                            && !it.label.contains('\n')
                            && it.detail.as_deref().is_none_or(|d| !d.contains('\n'))
                        {
                            1
                        } else {
                            let mut raw = it.label.clone();
                            if let Some(d) = &it.detail {
                                raw.push_str("  ");
                                raw.push_str(d);
                            }
                            wrap_line(&Line::raw(raw), content_width).len().max(1)
                        }
                    } else {
                        let checkbox = multi && it.is_selectable();
                        let content_width = inner.saturating_sub(if checkbox { 4 } else { 0 });
                        let hkey = self.select_row_key(it, i, vis_idx == selected, filter, content_width);
                        if let Some(&cached_h) = self.component_caches.select_row_heights.borrow().get(&hkey) {
                            cached_h
                        } else {
                            let (lines, _) = build_item(vis_idx, i, true);
                            let measured_h = lines.len().max(1);
                            let mut hc = self.component_caches.select_row_heights.borrow_mut();
                            if hc.len() >= 4096
                                && let Some(evict) = hc.keys().next().cloned()
                            {
                                hc.remove(&evict);
                            }
                            hc.insert(hkey, measured_h);
                            measured_h
                        }
                    };
                    total += h;
                }
                starts.push(total);
                let vis_h = cap_h.min(total.max(1)).max(1);
                let highlighted = visible.get(selected).filter(|&&i| items[i].is_selectable()).map(|_| selected);
                let sel = highlighted.unwrap_or(0).min(n - 1);
                let start = if total <= vis_h {
                    0
                } else {
                    let need = starts[sel + 1].saturating_sub(vis_h);
                    let top = starts.iter().copied().find(|&s| s >= need).unwrap_or(starts[sel]);
                    top.min(starts[sel]).min(total - vis_h)
                };
                let vis_lo = starts[..n].partition_point(|&s| s <= start).saturating_sub(1);
                let vis_hi = starts[..n].partition_point(|&s| s < start + vis_h);
                let mut window_rows: Vec<(usize, Vec<Line>)> = Vec::with_capacity(vis_hi - vis_lo);
                let mut need_full_fallback = false;
                for (offset, &i) in visible[vis_lo..vis_hi].iter().enumerate() {
                    let vis_idx = vis_lo + offset;
                    let expected_h = starts[vis_idx + 1] - starts[vis_idx];
                    let expected_preset_1 = items[i].lines.is_empty() && has_lua_row && is_preset;
                    let (lines, from_lua) = build_item(vis_idx, i, has_lua_row);
                    if lines.len().max(1) != expected_h || (expected_preset_1 && !from_lua) {
                        need_full_fallback = true;
                        break;
                    }
                    window_rows.push((vis_idx, lines));
                }
                if !need_full_fallback {
                    let empty = Line::empty();
                    let list: Vec<Line> = window_rows
                        .iter()
                        .flat_map(|(vis_idx, it_lines)| {
                            let slice: &[Line] =
                                if it_lines.is_empty() { std::slice::from_ref(&empty) } else { it_lines };
                            slice.iter().map(move |l| (l, *vis_idx))
                        })
                        .skip(start - starts[vis_lo])
                        .take(vis_h)
                        .map(|(l, vis_idx)| frame::list_row(l, highlighted == Some(vis_idx), true, inner, theme))
                        .collect();
                    return (list, frame::Scroll { top: start, visible: vis_h, total }, n);
                }
            }
            // Fallback for custom multi-line SelectRow or empty lists.
            let rows: Vec<Vec<Line>> =
                visible.iter().enumerate().map(|(vis_idx, &i)| build_item(vis_idx, i, true).0).collect();
            let total: usize = rows.iter().map(|r| r.len()).sum();
            let highlighted = visible.get(selected).filter(|&&i| items[i].is_selectable()).map(|_| selected);
            let (list, scroll) = frame::list_items(&rows, highlighted, true, cap_h.min(total.max(1)), inner, theme);
            (list, scroll, n)
        };
        let decode_frame = |comp: &str, title: &str, v: mlua::Value| -> Option<super::lua_components::ModalFrameSpec> {
            let mlua::Value::Table(t) = v else {
                self.record_component_failure(comp, "frame", "expected table");
                return None;
            };
            self.clear_component_failure(comp, "frame");
            let spans = |k: &str| {
                t.get::<mlua::Value>(k)
                    .ok()
                    .filter(|v| !v.is_nil())
                    .and_then(|v| mare_binding::ui::node_from(theme, v).ok())
                    .map(|n| super::lua_components::node_spans(n, theme))
            };
            let st = |k: &str, d: &str| t.get::<Option<String>>(k).ok().flatten().unwrap_or_else(|| d.into());
            Some(super::lua_components::ModalFrameSpec {
                title: spans("title")
                    .unwrap_or_else(|| vec![Span::styled(title.to_string(), theme.style("MareTitle"))]),
                border: st("border", "MareBorderActive"),
                footer_style: st("footer_style", "MareDim"),
                search: spans("search").unwrap_or_default(),
                cursor: spans("cursor").unwrap_or_default(),
                empty: Line::new(spans("empty").unwrap_or_default()),
            })
        };
        let mut body_scroll_max = 0;
        let mut preview_scroll_max = 0;
        let mut info_scroll_max = 0;
        match &self.modal {
            Some(Modal::Select { title, items, selected, kind, body_scroll, .. }) if kind == "confirm" => {
                // centered dialog: title, then [Y]es [N]o with the current one reversed (Yazi
                // confirm) — drawn by the Lua component `mare.ui.Confirm`, this as the fallback
                pop_w = w.saturating_sub(4).clamp(24, 60).min(w.max(1));
                let yes = items.first().map(|i| i.label.as_str()).unwrap_or("Yes");
                let no = items.get(1).map(|i| i.label.as_str()).unwrap_or("No");
                if let Some(rows) = self.confirm_rows(title, yes, no, *selected, pop_w, theme, frame)
                    && rows.len() <= max_h
                {
                    boxed = rows;
                } else {
                    let yes = items.first().map(|i| i.label.as_str()).unwrap_or("Yes");
                    let no = items.get(1).map(|i| i.label.as_str()).unwrap_or("No");
                    let fmt = |s: &str| match s.chars().next() {
                        Some(c) => format!("[{}]{}", c.to_uppercase(), &s[c.len_utf8()..]),
                        None => String::new(),
                    };
                    let inner = pop_w.saturating_sub(4).max(1);
                    let inner_max = max_h.saturating_sub(2).max(1);
                    let wrapped = wrap_line(&Line::raw(title.clone()), inner);
                    let reserved = if inner_max >= 4 {
                        3
                    } else if inner_max >= 2 {
                        1
                    } else {
                        0
                    };
                    let title_budget = inner_max.saturating_sub(reserved);
                    let total_wrapped = wrapped.len();
                    if inner_max >= 4 && total_wrapped <= title_budget {
                        let fb = mare_tui::fallback::Fallback::frame(
                            vec![Span::styled("Confirm", theme.style("MareTitle"))],
                            "MareBorderActive",
                            vec![
                                mare_tui::fallback::Fallback::lines(vec![
                                    Line::empty(),
                                    Line::raw(title.clone()),
                                    Line::empty(),
                                ]),
                                mare_tui::fallback::Fallback::choice(vec![fmt(yes), fmt(no)], *selected),
                            ],
                        );
                        boxed = mare_tui::fallback::render(&fb, pop_w, theme);
                    } else {
                        let buttons = mare_tui::fallback::choice_line(
                            &[fmt(yes), fmt(no)],
                            *selected,
                            mare_tui::fallback::Fallback::DEFAULT_GAP,
                            inner,
                            theme,
                        );
                        body_scroll_max = total_wrapped.saturating_sub(title_budget);
                        let sc = (*body_scroll).min(body_scroll_max);
                        let shown: Vec<Line> = wrapped.into_iter().skip(sc).take(title_budget).collect();
                        let mut content = vec![];
                        if inner_max >= 4 {
                            content.push(Line::empty());
                        }
                        content.extend(shown);
                        if inner_max >= 4 {
                            content.push(Line::empty());
                        }
                        content.push(buttons);
                        let mut o = frame::BoxOpts::for_theme(theme, theme.style("MareBorderActive"));
                        o.title = vec![Span::styled("Confirm", theme.style("MareTitle"))];
                        if total_wrapped > title_budget {
                            o.scroll = Some(frame::Scroll { top: sc, visible: title_budget, total: total_wrapped });
                            o.scrollbar_style = theme.style("MareScrollbar");
                        }
                        boxed = frame::boxed(&content, pop_w, &o);
                    }
                }
            }
            Some(Modal::Select {
                title,
                title_right,
                split,
                items,
                filter,
                selected,
                tabs,
                tab,
                searching,
                body,
                body_scroll,
                preview_scroll,
                preview_hidden,
                multi,
                filter_cache,
                ..
            }) => {
                let visible = filter_cache.get(items, *tab, filter);
                let preview = (!*preview_hidden)
                    .then(|| visible.get(*selected).and_then(|&i| items[i].preview.as_ref()))
                    .flatten();
                let wide = w >= 84;
                if preview.is_some() {
                    pop_w = w.saturating_sub(2).clamp(20, 140).min(w.max(1));
                }
                // the preview keeps at least 20 columns whatever the split asks
                let list_w = if preview.is_some() && wide {
                    ((pop_w as f32 * split.ratio) as usize)
                        .max(split.min)
                        .min(pop_w.saturating_sub(21))
                        .max(24.min(pop_w))
                } else {
                    pop_w
                };
                let mut head = vec![];
                let body_w = list_w.saturating_sub(4).max(8);
                if !body.is_empty() {
                    for b in body {
                        head.extend(mare_shared::style::wrap_line_hard(&Line::raw(b.clone()), body_w));
                    }
                    head.push(Line::empty());
                }
                let search_rows = usize::from(*searching || !filter.is_empty());
                let min_list_rows = visible.len().clamp(1, 6);
                let head_budget = max_h.saturating_sub(2 + search_rows + min_list_rows).max(1);
                body_scroll_max = head.len().saturating_sub(head_budget);
                let body_sc = (*body_scroll).min(body_scroll_max);
                let head_total = head.len();
                let mut head: Vec<Line> = head.into_iter().skip(body_sc).take(head_budget).collect();
                let remaining = head_total.saturating_sub(body_sc + head_budget);
                if remaining > 0 && head_budget >= 2 {
                    if head.len() == head_budget {
                        head.pop();
                    }
                    head.push(Line::styled(format!("… {} more detail lines", remaining + 1), theme.fg("MareDim")));
                }
                let chrome = 2 + search_rows + head.len();
                let (cap_h, pv_inner_max) = if preview.is_some() && !wide {
                    if max_h >= chrome + 4 {
                        let pv_box_h = (max_h / 3).clamp(3, 10).min(max_h - (chrome + 1));
                        (default_list_h.min(max_h - pv_box_h - chrome), pv_box_h - 2)
                    } else {
                        (default_list_h.min(max_h.saturating_sub(chrome).max(1)), 0)
                    }
                } else {
                    (default_list_h.min(max_h.saturating_sub(chrome).max(1)), 0)
                };
                let (mut rows, scroll, n) =
                    render_list(items, &visible, filter, *selected, list_w.saturating_sub(4).max(1), cap_h, *multi);
                let names: Vec<String> = tabs.iter().map(|t| t.name.clone()).collect();
                let fr = self.select_frame(
                    title,
                    &names,
                    *tab,
                    *searching || !filter.is_empty(),
                    items.is_empty() && filter.is_empty(),
                    list_w,
                    theme,
                );
                let head_len = head.len();
                if *searching || !filter.is_empty() {
                    let mut l = Line::new(fr.search.clone());
                    l.push(Span::raw(filter.clone()));
                    l.spans.extend(fr.cursor.iter().cloned());
                    rows.insert(0, l);
                    let inner_w = list_w.saturating_sub(4);
                    let col =
                        (Line::new(fr.search.clone()).width() + mare_shared::style::str_width(filter)).min(inner_w);
                    pop_cursor = Some((head_len, col));
                }
                if n == 0 {
                    rows.push(fr.empty.clone());
                }
                if !head.is_empty() {
                    rows.splice(0..0, head);
                }
                let mut o = frame::BoxOpts::for_theme(theme, theme.style(&fr.border));
                o.title = fr.title.clone();
                o.title_right = title_right.iter().map(|s| s.resolve(theme)).collect();
                o.footer = (n > 0).then(|| format!("{} of {n}", (*selected + 1).min(n)));
                o.footer_style = theme.style(&fr.footer_style);
                o.scroll = Some(scroll);
                o.scrollbar_style = theme.style("MareScrollbar");
                let list_box = frame::boxed(&rows, list_w, &o);
                boxed = match preview {
                    Some(pv) if wide || pv_inner_max > 0 => {
                        let pw = if wide { pop_w.saturating_sub(list_w + 1).max(1) } else { pop_w };
                        let lines = render_node(pv, pw.saturating_sub(4).max(1), theme, frame);
                        let max = if wide {
                            list_box.len().saturating_sub(2).max(3).min(max_h.saturating_sub(2).max(1))
                        } else {
                            pv_inner_max
                        };
                        let total = lines.len();
                        preview_scroll_max = total.saturating_sub(max);
                        let pv_sc = (*preview_scroll).min(preview_scroll_max);
                        let mut lines: Vec<Line> = lines.into_iter().skip(pv_sc).take(max).collect();
                        while wide && lines.len() < max {
                            lines.push(Line::empty());
                        }
                        let mut po = frame::BoxOpts::for_theme(theme, theme.style(&fr.preview_border));
                        po.title = fr.preview_title.clone();
                        if total > max {
                            po.scroll = Some(frame::Scroll { top: pv_sc, visible: max, total });
                            po.scrollbar_style = theme.style("MareScrollbar");
                        }
                        let pv_box = frame::boxed(&lines, pw, &po);
                        if wide {
                            frame::split(list_box, pv_box, 1)
                        } else {
                            let mut v = list_box;
                            v.extend(pv_box);
                            v
                        }
                    }
                    _ => list_box,
                };
            }
            Some(Modal::Help { items, filter, selected, .. }) => {
                let cap_h = default_list_h.min(max_h.saturating_sub(3).max(1));
                let visible = filter_items(items, filter);
                let (mut rows, scroll, n) =
                    render_list(items, &visible, filter, *selected, pop_w.saturating_sub(4).max(1), cap_h, false);
                let total = visible.iter().filter(|&&i| items[i].is_selectable()).count();
                let position =
                    visible.iter().take(selected.saturating_add(1)).filter(|&&i| items[i].is_selectable()).count();
                let searching = !filter.is_empty();
                let key = format!("{}|Help|{searching}", mare_shared::style::theme_gen());
                let arg = serde_json::json!({ "title": "Keys", "tabs": [], "tab": 1, "searching": searching });
                let fr = super::lua_components::cached(&self.component_caches.modal_frame, key, || {
                    self.lua_component("HelpModal", "frame", &[arg]).and_then(|v| decode_frame("HelpModal", "Keys", v))
                });
                if let Some(fr) = fr {
                    let mut l = Line::new(fr.search.clone());
                    l.push(Span::raw(filter.clone()));
                    l.spans.extend(fr.cursor.iter().cloned());
                    rows.insert(0, l);
                    let inner_w = pop_w.saturating_sub(4);
                    let col =
                        (Line::new(fr.search.clone()).width() + mare_shared::style::str_width(filter)).min(inner_w);
                    pop_cursor = Some((0, col));
                    if n == 0 {
                        rows.push(fr.empty.clone());
                    }
                    let mut o = frame::BoxOpts::for_theme(theme, theme.style(&fr.border));
                    o.title = fr.title;
                    o.footer = (total > 0).then(|| format!("{} of {total}", position.min(total)));
                    o.footer_style = theme.style(&fr.footer_style);
                    o.scroll = Some(scroll);
                    o.scrollbar_style = theme.style("MareScrollbar");
                    boxed = frame::boxed(&rows, pop_w, &o);
                } else {
                    rows.insert(
                        0,
                        Line::new(vec![Span::styled("/ ", theme.style("MareBorderSearch")), Span::raw(filter.clone())]),
                    );
                    let inner_w = pop_w.saturating_sub(4);
                    let col = (2 + mare_shared::style::str_width(filter)).min(inner_w);
                    pop_cursor = Some((0, col));
                    let fb = mare_tui::fallback::Fallback::frame(
                        vec![Span::styled("Keys", theme.style("MareTitle"))],
                        if searching { "MareBorderSearch" } else { "MareBorderActive" },
                        vec![mare_tui::fallback::Fallback::lines(rows)],
                    );
                    boxed = mare_tui::fallback::render(&fb, pop_w, theme);
                }
            }
            Some(Modal::Input { title, editor, secret, placeholder, .. }) => {
                let iw = pop_w.saturating_sub(4).max(1);
                let opts = EditorRenderOpts {
                    placeholder: placeholder.clone(),
                    max_height: max_h.saturating_sub(2).max(1),
                    mask_char: secret.then_some(if theme.icons == "ascii" { '*' } else { '•' }),
                    ..Default::default()
                };
                let r = editor.render(iw, theme, &opts);
                pop_cursor = Some((r.cursor.0 as usize, r.cursor.1 as usize));
                let fr = if *secret {
                    None
                } else {
                    let key = format!("{}|Input|{title}|{placeholder}", mare_shared::style::theme_gen());
                    let arg = serde_json::json!({ "title": title, "placeholder": placeholder, "secret": false });
                    super::lua_components::cached(&self.component_caches.modal_frame, key, || {
                        self.lua_component("InputModal", "frame", &[arg])
                            .and_then(|v| decode_frame("InputModal", title, v))
                    })
                };
                if let Some(fr) = fr {
                    let mut o = frame::BoxOpts::for_theme(theme, theme.style(&fr.border));
                    o.title = fr.title;
                    boxed = frame::boxed(&r.lines, pop_w, &o);
                } else {
                    let fb = mare_tui::fallback::Fallback::frame(
                        vec![Span::styled(title.clone(), theme.style("MareTitle"))],
                        "MareBorderActive",
                        vec![mare_tui::fallback::Fallback::lines(r.lines)],
                    );
                    boxed = mare_tui::fallback::render(&fb, pop_w, theme);
                }
            }
            Some(Modal::Info { title, node, scroll, cursor, .. }) => {
                let lines = render_node(node, pop_w.saturating_sub(4).max(1), theme, frame);
                let max = self.height.saturating_sub(8).max(5).min(max_h.saturating_sub(2).max(1));
                let sc = (*scroll).min(lines.len().saturating_sub(max));
                info_scroll_max = lines.len().saturating_sub(max);
                let shown: Vec<Line> = lines.iter().skip(sc).take(max).cloned().collect();
                if let Some((cr, cc)) = cursor
                    && !shown.is_empty()
                    && *cr >= sc
                    && *cr - sc < shown.len()
                {
                    let inner_w = pop_w.saturating_sub(4);
                    pop_cursor = Some((*cr - sc, (*cc).min(inner_w)));
                }
                let key =
                    format!("{}|Info|{title}|{}|{}|{sc}", mare_shared::style::theme_gen(), lines.len(), shown.len());
                let arg =
                    serde_json::json!({ "title": title, "total": lines.len(), "shown": shown.len(), "scroll": sc });
                if let Some(fr) = super::lua_components::cached(&self.component_caches.modal_frame, key, || {
                    self.lua_component("InfoModal", "frame", &[arg]).and_then(|v| decode_frame("InfoModal", title, v))
                }) {
                    let mut o = frame::BoxOpts::for_theme(theme, theme.style(&fr.border));
                    o.title = fr.title;
                    if lines.len() > max {
                        o.scroll = Some(frame::Scroll { top: sc, visible: max, total: lines.len() });
                        o.scrollbar_style = theme.style("MareScrollbar");
                        o.footer = Some(format!("{} of {}", (sc + max).min(lines.len()), lines.len()));
                        o.footer_style = theme.style(&fr.footer_style);
                    }
                    boxed = frame::boxed(&shown, pop_w, &o);
                } else {
                    let fb = mare_tui::fallback::Fallback::frame(
                        vec![Span::styled(title.clone(), theme.style("MareTitle"))],
                        "MareBorderActive",
                        vec![mare_tui::fallback::Fallback::lines(shown)],
                    );
                    boxed = mare_tui::fallback::render(&fb, pop_w, theme);
                }
            }
            None => {}
        }
        match &mut self.modal {
            Some(Modal::Select {
                body_scroll,
                body_scroll_max: bmax,
                preview_scroll,
                preview_scroll_max: pmax,
                ..
            }) => {
                *bmax = Some(body_scroll_max);
                *body_scroll = (*body_scroll).min(body_scroll_max);
                *pmax = Some(preview_scroll_max);
                *preview_scroll = (*preview_scroll).min(preview_scroll_max);
            }
            Some(Modal::Info { scroll, scroll_max, .. }) => {
                *scroll_max = Some(info_scroll_max);
                *scroll = (*scroll).min(info_scroll_max);
            }
            _ => {}
        }
        (boxed, pop_cursor, pop_w)
    }
}
