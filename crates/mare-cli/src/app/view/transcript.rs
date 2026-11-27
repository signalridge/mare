//! The Ctrl+T transcript overlay.

use crate::app::*;

pub(in crate::app) struct TranscriptCache {
    width: usize,
    theme_gen: u64,
    theme: Theme,
    /// blocks rendered into `prefix`
    blocks: usize,
    prefix: std::sync::Arc<Vec<Line>>,
    ranges: std::sync::Arc<Vec<(usize, usize, usize)>>,
}

/// A stable prefix shared with the cache plus the freshly rendered live suffix.
struct TranscriptLines {
    prefix: std::sync::Arc<Vec<Line>>,
    suffix: Vec<Line>,
    ranges: Vec<(usize, usize, usize)>,
    cached_ranges: Option<std::sync::Arc<Vec<(usize, usize, usize)>>>,
}

impl TranscriptLines {
    fn len(&self) -> usize {
        self.prefix.len() + self.suffix.len()
    }

    fn iter(&self) -> impl Iterator<Item = &Line> {
        self.prefix.iter().chain(self.suffix.iter())
    }

    fn find(&self, query: &str) -> Vec<usize> {
        let mut matches = mare_tui::search::find(&self.prefix, query);
        matches.extend(mare_tui::search::find(&self.suffix, query).into_iter().map(|i| self.prefix.len() + i));
        matches
    }

    fn first_intersecting(&self, top: usize) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        if let Some(cr) = &self.cached_ranges {
            let start = cr.partition_point(|&(_, s, c)| s + c <= top);
            out.extend_from_slice(&cr[start..]);
        }
        let start = self.ranges.partition_point(|&(_, s, c)| s + c <= top);
        out.extend_from_slice(&self.ranges[start..]);
        out
    }
}

/// The transcript layer's core actions (default keys in preset/keymap.lua).
const VIEW_ACTIONS: [&str; 12] = [
    "view.close",
    "view.back",
    "view.up",
    "view.down",
    "view.page_up",
    "view.page_down",
    "view.top",
    "view.bottom",
    "view.search",
    "view.copy",
    "view.next_match",
    "view.prev_match",
];

#[derive(Default)]
pub(in crate::app) struct TranscriptSearch {
    query: String,
    typing: bool,
    matches: Vec<usize>,
    dirty: bool,
    width: usize,
    theme_gen: u64,
    lines_len: usize,
}

/// The Ctrl+T transcript overlay's state (written only here).
#[derive(Default)]
pub(in crate::app) struct TranscriptView {
    /// open: its top line index
    pub(in crate::app) top: Option<usize>,
    /// the last drawn rows (diffed against the next frame) and their width
    pub(in crate::app) frame: Vec<String>,
    frame_width: usize,
    /// the overlay shows the terminal cursor (an input modal over it)
    cursor: bool,
    /// plugin-supplied content (mare.ui.transcript) shown instead of the blocks
    src: Option<(String, Node)>,
    /// the plugin view's own header keys (`ui.transcript(t, node, { keys })`)
    keys: Vec<mare_shared::ui::ViewKey>,
    /// rendered prefix (committed blocks, which can't change) cached while the overlay is open;
    /// only the live suffix is re-rendered per frame
    cache: Option<TranscriptCache>,
    /// search: query, whether it's being typed, matching line indexes
    search: Option<TranscriptSearch>,
    /// whether the visible block viewport in regular transcript currently intersects an animated block
    visible_anim: Option<(bool, usize, usize)>,
}

impl TranscriptView {
    pub(in crate::app) fn invalidate_cache(&mut self) {
        self.cache = None;
        self.visible_anim = None;
        self.invalidate_search();
    }

    pub(in crate::app) fn invalidate_search(&mut self) {
        if let Some(se) = self.search.as_mut() {
            se.dirty = true;
        }
    }

    pub(in crate::app) fn invalidate_anim(&mut self) {
        self.visible_anim = None;
    }
    pub(in crate::app) fn is_open(&self) -> bool {
        self.top.is_some()
    }

    pub(in crate::app) fn cancel_typing_search(&mut self) -> bool {
        if self.search.as_ref().is_some_and(|search| search.typing) {
            self.search = None;
            true
        } else {
            false
        }
    }

    /// Closed: everything but the last frame's width cleared. True when the overlay showed the
    /// terminal cursor (to hide again).
    fn reset(&mut self) -> bool {
        self.keys.clear();
        self.frame.clear();
        self.top = None;
        self.cache = None;
        self.search = None;
        self.src = None;
        self.visible_anim = None;
        std::mem::take(&mut self.cursor)
    }

    pub(super) fn src_has_animation(&self) -> bool {
        self.src.as_ref().is_some_and(|(_, n)| n.has_animation())
    }
}

impl App {
    pub(super) fn transcript_has_visible_animation(&self) -> bool {
        if self.transcript.src.is_some() {
            return self.transcript.src_has_animation();
        }
        let page = self.height.saturating_sub(1).max(1);
        let top = self.transcript.top.unwrap_or(0);
        let rows = if self.transcript.search.is_some() { page.saturating_sub(1).max(1) } else { page };
        if let Some((anim, cached_top, cached_rows)) = self.transcript.visible_anim
            && cached_top == top
            && cached_rows == rows
        {
            return anim;
        }
        self.blocks.iter().any(|b| b.has_animation(true, self.show_reasoning))
    }
    // --------------------------------------------------------------
    // Ctrl+T transcript (alternate screen, expanded content)
    // --------------------------------------------------------------
    fn transcript_lines(&mut self) -> TranscriptLines {
        let theme = self.theme();
        let w = self.width.saturating_sub(2).max(20);
        let frame = self.anim_frame();
        let help = self.view_default_help();
        if let Some((title, node)) = &self.transcript.src {
            let help = self.view_keys_help().unwrap_or(help);
            let mut lines = vec![Line::styled(format!(" {title} — {help}"), theme.fg("dim")), Line::empty()];
            let _measure = mare_tui::image::measure_only();
            lines.extend(render_node(node, w, &theme, frame).into_iter().map(|l| l.prefixed(vec![Span::raw(" ")])));
            return TranscriptLines { prefix: Default::default(), suffix: lines, ranges: vec![], cached_ranges: None };
        }
        // committed blocks never change: render them once per open overlay (and width/theme)
        let theme_gen = mare_shared::style::theme_gen();
        let committed = self.committed.min(self.blocks.len());
        let stale = self
            .transcript
            .cache
            .as_ref()
            .is_none_or(|c| c.width != w || c.theme_gen != theme_gen || c.theme != *theme || c.blocks > committed);
        if stale {
            self.transcript.visible_anim = None;
            self.transcript.cache = Some(TranscriptCache {
                width: w,
                theme_gen,
                theme: (*theme).clone(),
                blocks: 0,
                ranges: Default::default(),
                prefix: std::sync::Arc::new(vec![
                    Line::styled(format!(" Transcript (expanded) — {help}"), theme.fg("dim")),
                    Line::empty(),
                ]),
            });
        }
        let c = self.transcript.cache.as_mut().expect("just set");
        let _measure = mare_tui::image::measure_only();
        for (index, b) in self.blocks[c.blocks..committed].iter_mut().enumerate() {
            let prefix = std::sync::Arc::make_mut(&mut c.prefix);
            let ranges = std::sync::Arc::make_mut(&mut c.ranges);
            let start = prefix.len();
            prefix.extend(
                b.render(w, &theme, frame, true, self.show_reasoning, None)
                    .into_iter()
                    .map(|l| l.prefixed(vec![Span::raw(" ")])),
            );
            ranges.push((c.blocks + index, start, prefix.len() - start));
            prefix.push(Line::empty());
        }
        c.blocks = committed;
        let mut suffix = Vec::new();
        let mut ranges = Vec::with_capacity(self.blocks.len().saturating_sub(committed));
        for (index, b) in self.blocks[committed..].iter_mut().enumerate() {
            let start = c.prefix.len() + suffix.len();
            suffix.extend(
                b.render(w, &theme, frame, true, self.show_reasoning, None)
                    .into_iter()
                    .map(|l| l.prefixed(vec![Span::raw(" ")])),
            );
            ranges.push((committed + index, start, c.prefix.len() + suffix.len() - start));
            suffix.push(Line::empty());
        }
        TranscriptLines { prefix: c.prefix.clone(), suffix, ranges, cached_ranges: Some(c.ranges.clone()) }
    }

    pub(in crate::app) fn transcript_chain(&self) -> Vec<&'static str> {
        if self.transcript.search.as_ref().is_some_and(|se| se.typing) {
            vec!["transcript.search", "transcript", "global"]
        } else {
            vec!["transcript", "global"]
        }
    }

    /// Ctrl+T: expanded transcript on the alternate screen. Non-blocking — the event loop keeps
    /// running (streaming, agent, plugins); content refreshes as blocks change.
    pub(in crate::app) fn transcript_overlay(&mut self) {
        if self.select.active() {
            self.leave_select_mode();
        }
        self.pending_prefix = None;
        self.transcript.frame.clear();
        mare_tui::modes::set_alt_screen(true);
        let n = self.transcript_lines().len();
        self.transcript.top = Some(n.saturating_sub(self.height.saturating_sub(1)));
    }

    /// A plugin view's header help from its own keys (`ui.transcript(t, node, { keys })`): the
    /// keys bound to each entry's actions in the transcript layer now, then the scrolling and
    /// search keys every view has. None without keys (the default text).
    fn view_keys_help(&self) -> Option<String> {
        if self.transcript.keys.is_empty() {
            return None;
        }
        let mut parts = vec![];
        for k in &self.transcript.keys {
            let chain = self.transcript_chain();
            let keys: Vec<String> = k
                .actions
                .iter()
                .filter_map(|a| self.keymap.hint(a, &chain).first().map(|k| mare_shared::bindings::key_label(k)))
                .collect();
            if !keys.is_empty() {
                parts.push(format!("{} {}", keys.join("/"), k.label));
            }
        }
        parts.push(self.view_scroll_help());
        Some(parts.join(" · "))
    }

    /// The label of the key bound to `action` in the transcript layer: the `preferred` key when
    /// it is still bound to it (the header's usual wording), else the first key bound now.
    fn view_key_label(&self, action: &str, preferred: &str) -> Option<String> {
        let chain = self.transcript_chain();
        let keys = self.keymap.hint(action, &chain);
        let key = if keys.contains(&preferred) { preferred } else { keys.first().copied()? };
        Some(match key {
            "up" => "↑".into(),
            "down" => "↓".into(),
            "pageup" => "PgUp".into(),
            "pagedown" => "PgDn".into(),
            // sentence hint: bare letters stay as typed (shift+g reads "G")
            k => match k.strip_prefix("shift+") {
                Some(c) if c.chars().count() == 1 => c.to_uppercase(),
                _ if k.chars().count() == 1 => k.to_string(),
                _ => mare_shared::bindings::key_label(k),
            },
        })
    }

    /// The scrolling and search keys every view has, from the transcript layer's bindings.
    fn view_scroll_help(&self) -> String {
        let pair = |a: (&str, &str), b: (&str, &str)| {
            Some(format!("{}/{}", self.view_key_label(a.0, a.1)?, self.view_key_label(b.0, b.1)?))
        };
        let mut parts: Vec<String> = vec![];
        parts.extend(pair(("view.up", "up"), ("view.down", "down")));
        parts.extend(pair(("view.page_up", "pageup"), ("view.page_down", "pagedown")));
        parts.extend(pair(("view.top", "g"), ("view.bottom", "shift+g")));
        let mut groups = vec![parts.join(" ")];
        for (action, key, word) in [("view.search", "/", "search"), ("view.copy", "y", "copy")] {
            if action == "view.copy" && self.transcript.src.is_some() {
                continue;
            }
            groups.extend(self.view_key_label(action, key).map(|k| format!("{k} {word}")));
        }
        groups.retain(|g| !g.is_empty());
        groups.join(", ")
    }

    /// The built-in transcript views' header help (Ctrl+T and any overlay without its own keys).
    fn view_default_help(&self) -> String {
        let scroll = self.view_scroll_help();
        match self.view_key_label("view.back", "esc") {
            Some(k) => format!("{scroll}, {k} to close"),
            None => scroll,
        }
    }

    /// `mare.ui.transcript(title, node, { keys })`: a plugin's content in the overlay (opened, or
    /// refreshed in place when it already shows that title; at the end it keeps following).
    pub(in crate::app) fn show_plugin_transcript(
        &mut self,
        title: String,
        node: Node,
        keys: Vec<mare_shared::ui::ViewKey>,
    ) {
        let same = self.transcript.is_open() && self.transcript.src.as_ref().is_some_and(|(t, _)| *t == title);
        // following the end (live tail) keeps following after a refresh
        let page = self.height.saturating_sub(1).max(1);
        let at_end = same && self.transcript.top.unwrap_or(0) + page >= self.transcript_lines().len();
        *self.ui.transcript_title.write().unwrap() = Some(title.clone());
        self.transcript.src = Some((title, node));
        self.transcript.keys = keys;
        self.transcript.invalidate_search();
        if !same {
            self.transcript_overlay();
        } else if at_end {
            let n = self.transcript_lines().len();
            self.transcript.top = Some(n.saturating_sub(page));
        }
    }

    /// `mare.ui.transcript_close(title)`: closes the overlay when it shows that plugin view (any
    /// plugin view without a title).
    pub(in crate::app) fn close_plugin_transcript(&mut self, title: Option<String>) {
        let open = self.transcript.is_open()
            && self.transcript.src.as_ref().is_some_and(|(t, _)| title.as_ref().is_none_or(|w| w == t));
        if open {
            self.close_transcript();
        }
    }

    pub(in crate::app) fn close_transcript(&mut self) {
        self.pending_prefix = None;
        let had_cursor = self.transcript.reset();
        *self.ui.transcript_title.write().unwrap() = None;
        if had_cursor && !mare_tui::modes::handed_off() {
            mare_tui::out::write(b"\x1b[?25l".to_vec());
        }
        mare_tui::modes::set_alt_screen(false);
        self.renderer.invalidate();
        self.invalidate_cpr();
        self.sync_position_known();
    }

    /// Ctrl+T `y`: the message at the reading position (a third down the page, like search) as
    /// `copy/request {text, kind, from = "transcript"}` for the `copy` plugin —
    /// the Markdown source when the block has one, else its text.
    fn copy_transcript_message(&mut self) {
        if self.transcript.src.is_some() {
            return;
        }
        let page = self.height.saturating_sub(1).max(1);
        let at = self.transcript.top.unwrap_or(0) + page / 3;
        let lines = self.transcript_lines();
        let found = {
            let it = lines.first_intersecting(at.saturating_sub(1));
            it.into_iter()
                .find(|&(_, start, count)| at < start + count + 1)
                .map(|(block, _, _)| block)
                .or_else(|| self.blocks.len().checked_sub(1))
        };
        let theme = self.theme();
        let w = self.width.saturating_sub(2).max(20);
        let frame = self.anim_frame();
        let show_reasoning = self.show_reasoning;
        let Some(b) = found.and_then(|i| self.blocks.get_mut(i)) else { return };
        let text = match &b.md {
            Some(md) => md.text().to_string(),
            None => b
                .render(w, &theme, frame, true, show_reasoning, None)
                .iter()
                .map(|l| l.plain())
                .collect::<Vec<_>>()
                .join("\n"),
        };
        if text.trim().is_empty() {
            return;
        }
        let kind = b.kind.clone();
        self.emit("copy/request", serde_json::json!({ "text": text, "kind": kind, "from": "transcript" }));
    }

    pub(in crate::app) fn handle_transcript_key(&mut self, k: KeyEvent) {
        let page = self.height.saturating_sub(1).max(1);
        let ks = key_to_string(&k);
        let typing = self.transcript.search.as_ref().is_some_and(|se| se.typing);
        let active_chain = self.transcript_chain();
        if self.complete_prefix(KeyFocus::Transcript, &k, &ks, &active_chain) {
            return;
        }
        // typing a search query: `transcript.search` -> printable -> query -> Esc/Backspace/Enter ->
        // `transcript` -> `global` (a mask in `transcript.search` stops the whole chain)
        if typing {
            if self.try_prefix(&["transcript.search"], &ks, KeyFocus::Transcript) {
                return;
            }
            if self.dispatch_bindings_stops(&["transcript.search"], &ks, false) {
                return;
            }
            if let Some(c) = mare_tui::keys::printable_char(&k) {
                if let Some(se) = self.transcript.search.as_mut() {
                    se.query.push(c);
                }
                return;
            }
            match k.code {
                KeyCode::Escape => {
                    self.transcript.search = None;
                    return;
                }
                KeyCode::Backspace => {
                    if let Some(se) = self.transcript.search.as_mut() {
                        se.query.pop();
                    }
                    return;
                }
                KeyCode::Enter => {
                    let lines = self.transcript_lines();
                    let max_top = lines.len().saturating_sub(page);
                    let width = self.width;
                    let tgen = mare_shared::style::theme_gen();
                    let se = self.transcript.search.as_mut().expect("checked");
                    se.typing = false;
                    se.matches = lines.find(&se.query);
                    se.dirty = false;
                    se.width = width;
                    se.theme_gen = tgen;
                    se.lines_len = lines.len();
                    let from = self.transcript.top.unwrap_or(0);
                    if let Some(m) = mare_tui::search::step(&se.matches, from, false) {
                        self.transcript.top = Some(m.saturating_sub(page / 3).min(max_top));
                    }
                    return;
                }
                _ => {}
            }
            let tail: Vec<String> = self
                .keymap
                .chain_for("transcript")
                .map(|c| c.into_iter().map(str::to_owned).collect())
                .unwrap_or_default();
            let tail_refs: Vec<&str> = tail.iter().map(String::as_str).collect();
            if self.try_prefix(&tail_refs, &ks, KeyFocus::Transcript) {
                return;
            }
            self.run_bindings(&tail_refs, &ks, false);
            return;
        }
        let chain = self.transcript_chain();
        if self.try_prefix(&chain, &ks, KeyFocus::Transcript) {
            return;
        }
        self.run_bindings(&chain, &ks, false);
    }

    /// The transcript layer's core actions (`view.*`); None for any other name. False when the
    /// action doesn't apply (no overlay open, `n` with no search), so the key goes on.
    pub(in crate::app) fn view_action(&mut self, action: &str) -> Option<bool> {
        if !VIEW_ACTIONS.contains(&action) {
            return None;
        }
        if !self.transcript.is_open() {
            return Some(false);
        }
        let page = self.height.saturating_sub(1).max(1);
        match action {
            "view.search" => self.transcript.search = Some(TranscriptSearch { typing: true, ..Default::default() }),
            "view.copy" => self.copy_transcript_message(),
            "view.next_match" | "view.prev_match" => {
                if self.transcript.search.is_none() {
                    return Some(false);
                }
                let lines = self.transcript_lines();
                self.refresh_transcript_search(&lines);
                let max_top = lines.len().saturating_sub(page);
                let se = self.transcript.search.as_ref().expect("checked");
                let back = action == "view.prev_match";
                // the match at the reading position (a third down the page) is the current one
                let cur = self.transcript.top.unwrap_or(0) + page / 3;
                let from = if back { cur } else { cur + 1 };
                if let Some(m) = mare_tui::search::step(&se.matches, from, back) {
                    self.transcript.top = Some(m.saturating_sub(page / 3).min(max_top));
                }
            }
            // Esc: an active search is cleared first, then the overlay closes; `q` / Ctrl+T close
            // it at once
            "view.back" if self.transcript.search.is_some() => self.transcript.search = None,
            "view.back" | "view.close" => self.close_transcript(),
            _ => {
                let max_top = self.transcript_lines().len().saturating_sub(page);
                let top = self.transcript.top.as_mut().expect("checked");
                *top = match action {
                    "view.up" => top.saturating_sub(1),
                    "view.down" => (*top + 1).min(max_top),
                    "view.page_up" => top.saturating_sub(page),
                    "view.page_down" => (*top + page).min(max_top),
                    "view.top" => 0,
                    "view.bottom" => max_top,
                    _ => return None,
                };
            }
        }
        Some(true)
    }

    fn refresh_transcript_search(&mut self, lines: &TranscriptLines) {
        let width = self.width;
        let tgen = mare_shared::style::theme_gen();
        let len = lines.len();
        if let Some(se) = self.transcript.search.as_mut()
            && !se.typing
            && (se.dirty || se.width != width || se.theme_gen != tgen || se.lines_len != len)
        {
            se.matches = lines.find(&se.query);
            se.dirty = false;
            se.width = width;
            se.theme_gen = tgen;
            se.lines_len = len;
        }
    }

    pub(super) fn render_transcript(&mut self) {
        self.keymap.sync_from(&self.ui.keymap.read().unwrap());
        let lines = self.transcript_lines();
        self.refresh_transcript_search(&lines);
        let page = self.height.saturating_sub(1).max(1);
        let top = self.transcript.top.unwrap_or(0).min(lines.len().saturating_sub(page));
        let color = self.renderer.color;
        let theme = self.theme();
        // a search reserves the last row for its prompt / match count
        let (rows, status) = match &self.transcript.search {
            Some(se) => {
                let text = if se.typing {
                    format!("/{}", se.query)
                } else if se.matches.is_empty() {
                    format!("/{}  no matches", se.query)
                } else {
                    let cur = top + page / 3;
                    let target = cur.min(top + page);
                    let pos = se.matches.partition_point(|&m| m < target);
                    let i = if pos < se.matches.len() { pos } else { 0 };
                    format!("/{}  {} of {}  n/N next/previous, Esc clears", se.query, i + 1, se.matches.len())
                };
                (page.saturating_sub(1).max(1), Some(Line::styled(text, theme.style("MareAccent"))))
            }
            None => (page, None),
        };
        let query = self.transcript.search.as_ref().filter(|se| !se.typing).map(|se| se.query.as_str()).unwrap_or("");
        let hl = theme.style("MareAccent").reverse();
        // the screen as rows of bytes, diffed against the last frame: scrolling or a streaming
        // block rewrites only the rows that changed, an unchanged frame writes nothing
        let mut screen: Vec<Line> = vec![Line::empty(); page + 1];
        for (i, l) in lines.iter().skip(top).take(rows).enumerate() {
            screen[i] = if query.is_empty() { l.clone() } else { mare_tui::search::highlight(l, query, hl) };
        }
        // History retains layout/text only; materialize pictures for intersecting blocks.
        let frame = self.anim_frame();
        let width = self.width.saturating_sub(2).max(1);
        if let Some((_, node)) = &self.transcript.src {
            let _window = mare_tui::image::window(top.saturating_sub(2), (top + rows).saturating_sub(2));
            let actual = render_node(node, width, &theme, frame);
            for (index, line) in actual.into_iter().enumerate() {
                let at = index + 2;
                if at >= top && at < top + rows {
                    let prefixed = line.prefixed(vec![Span::raw(" ")]);
                    screen[at - top] =
                        if query.is_empty() { prefixed } else { mare_tui::search::highlight(&prefixed, query, hl) };
                }
            }
        } else {
            let committed = self.committed.min(self.blocks.len());
            let mut visible_anim = false;
            for (block, start, _count) in lines.first_intersecting(top) {
                if start >= top + rows {
                    break;
                }
                let anim = self.blocks[block].has_animation(true, self.show_reasoning);
                visible_anim |= anim;
                if self.blocks[block].has_images(true, self.show_reasoning) || (block < committed && anim) {
                    let _window =
                        mare_tui::image::window(top.saturating_sub(start), (top + rows).saturating_sub(start));
                    let actual = self.blocks[block].render(width, &theme, frame, true, self.show_reasoning, None);
                    for (index, line) in actual.into_iter().enumerate() {
                        let at = start + index;
                        if at >= top && at < top + rows {
                            let prefixed = line.prefixed(vec![Span::raw(" ")]);
                            screen[at - top] = if query.is_empty() {
                                prefixed
                            } else {
                                mare_tui::search::highlight(&prefixed, query, hl)
                            };
                        }
                    }
                }
            }
            self.transcript.visible_anim = Some((visible_anim, top, rows));
        }
        if let Some(st) = status {
            screen[page] = st;
        }
        let cursor = self.overlay_modal(&mut screen, &theme);
        let w = self.width.max(1);
        let set = frame::BorderSet::for_theme(&theme.border, &theme.icons);
        if let Some((wk_box, _)) = self.which_key_box(w, &theme, set) {
            let last = screen.len().saturating_sub(1);
            let bw = wk_box.iter().map(Line::width).max().unwrap_or(0);
            let x = w.saturating_sub(bw) / 2;
            let shown = wk_box.len().min(last);
            let top = last - shown;
            for (i, b) in wk_box.iter().skip(wk_box.len() - shown).enumerate() {
                screen[top + i] = overlay_line(&screen[top + i], x, b);
            }
        }
        self.flush_component_notices();
        self.notices.prepare(app_now(), self.modal.is_some());
        if !self.notices.is_empty() {
            let mut no_cursor = None;
            self.overlay_notices(&mut screen, None, self.width, &theme, set, &mut no_cursor);
        }
        let frame: Vec<String> = screen.iter().map(|l| line_to_ansi(l, self.width, color)).collect();
        let full = self.transcript.frame.len() != frame.len() || self.transcript.frame_width != self.width;
        let mut s = String::new();
        for (i, row) in frame.iter().enumerate() {
            if !full && self.transcript.frame.get(i) == Some(row) {
                continue;
            }
            // erase after an SGR reset, so no leftover background paints the cleared cells
            let _ = std::fmt::Write::write_fmt(&mut s, format_args!("\x1b[{};1H\x1b[0m\x1b[2K", i + 1));
            s.push_str(row);
        }
        if full {
            s.insert_str(0, "\x1b[H\x1b[0m\x1b[2J");
        }
        self.transcript.frame = frame;
        self.transcript.frame_width = self.width;
        // an input modal's cursor (the ask dialog's reason): parked after the rows are written,
        // hidden again once the modal closes
        match cursor {
            Some((r, c)) if !s.is_empty() || !self.transcript.cursor => {
                let _ = std::fmt::Write::write_fmt(&mut s, format_args!("\x1b[{};{}H\x1b[?25h", r + 1, c + 1));
                self.transcript.cursor = true;
            }
            None if self.transcript.cursor => {
                s.push_str("\x1b[?25l");
                self.transcript.cursor = false;
            }
            _ => {}
        }
        if s.is_empty() {
            return;
        }
        if self.renderer.caps.sync {
            s.insert_str(0, "\x1b[?2026h");
            s.push_str("\x1b[?2026l");
        }
        mare_tui::out::write(s.into_bytes());
        if self.modal.is_some() {
            self.mark_modal_drawn();
        }
    }

    /// The open modal drawn over the overlay's bottom rows, its key hints on the last row
    /// (a modal is on top of whatever is on screen, the transcript or a child's view,
    /// and takes the keys; the overlay stays open underneath). Returns the cursor of an input.
    fn overlay_modal(&mut self, screen: &mut [Line], theme: &Theme) -> Option<(usize, usize)> {
        if self.modal.is_none() || screen.is_empty() {
            return None;
        }
        let (w, frame) = (self.width.max(1), self.anim_frame());
        let max_h = self.height.saturating_sub(1).max(4);
        let (boxed, cursor, _) = self.modal_box(w, max_h, theme, frame);
        let last = screen.len() - 1;
        screen[last] = self.status_line(w, theme, frame, false);
        let bw = boxed.iter().map(Line::width).max().unwrap_or(0);
        let x = w.saturating_sub(bw) / 2;
        let shown = boxed.len().min(last);
        let top = last - shown;
        // too tall for the screen: its bottom (the options, the input) stays visible
        for (i, b) in boxed.iter().skip(boxed.len() - shown).enumerate() {
            screen[top + i] = overlay_line(&screen[top + i], x, b);
        }
        let skipped = boxed.len() - shown;
        cursor.and_then(|(r, c)| (r + 1).checked_sub(skipped).map(|r| (top + r, x + 2 + c)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_pages_and_search_cross_the_shared_prefix_boundary() {
        let prefix = std::sync::Arc::new(vec![Line::raw("first Needle"), Line::raw("second")]);
        let lines = TranscriptLines {
            prefix: prefix.clone(),
            ranges: vec![],
            cached_ranges: None,
            suffix: vec![Line::raw("third needle"), Line::raw("fourth Needle")],
        };
        assert_eq!(lines.len(), 4);
        assert_eq!(lines.iter().skip(1).take(2).map(Line::plain).collect::<Vec<_>>(), ["second", "third needle"]);
        assert_eq!(lines.find("needle"), [0, 2, 3]);
        assert_eq!(lines.find("Needle"), [0, 3]);
        assert!(lines.find("").is_empty());
        assert_eq!(prefix.len(), 2);
    }

    #[test]
    fn transcript_binary_search_finds_exact_intersecting_blocks_across_cached_and_live() {
        let cached = std::sync::Arc::new(vec![(0, 2, 3), (1, 6, 4), (2, 11, 2)]);
        let live = vec![(3, 14, 5), (4, 20, 3)];
        let lines = TranscriptLines {
            prefix: std::sync::Arc::new(vec![Line::empty(); 14]),
            suffix: vec![Line::empty(); 10],
            ranges: live,
            cached_ranges: Some(cached),
        };
        // Viewport top=7 should skip block 0 (ends at 5) and start at block 1 (6..10).
        let intersecting = lines.first_intersecting(7);
        assert_eq!(intersecting, vec![(1, 6, 4), (2, 11, 2), (3, 14, 5), (4, 20, 3)]);
        // Viewport top=15 skips all cached blocks and starts at live block 3.
        assert_eq!(lines.first_intersecting(15), vec![(3, 14, 5), (4, 20, 3)]);
    }

    #[test]
    fn transcript_search_highlight_preserves_image_raw_command_alongside_text_match() {
        let mut img_row = Line::raw(" ");
        img_row.raw = Some("\x1b_Ga=T,i=99;AAAA\x1b\\".into());
        let text_row = Line::raw("matching needle text");
        let hl = Style::new().reverse();
        let out_img = mare_tui::search::highlight(&img_row, "needle", hl);
        let out_txt = mare_tui::search::highlight(&text_row, "needle", hl);
        assert_eq!(out_img.raw.as_deref(), Some("\x1b_Ga=T,i=99;AAAA\x1b\\"));
        assert_eq!(out_txt.plain(), "matching needle text");
        assert!(out_txt.spans.iter().any(|s| s.text == "needle" && s.style == hl));
    }
}
