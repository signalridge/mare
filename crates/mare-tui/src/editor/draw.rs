//! Drawing: the rounded input box with prompt, placeholder, scrolling and token highlighting.

use super::*;

impl Editor {
    pub fn render(&self, width: usize, theme: &Theme, opts: &EditorRenderOpts) -> EditorRender {
        if let Some(mc) = opts.mask_char {
            return self.render_masked(width, theme, opts, mc);
        }
        let border = theme.fg(opts.border_color.as_deref().unwrap_or("border"));
        let accent = theme.fg(opts.prompt_style.as_deref().unwrap_or("accent"));
        // the box follows `mare.o.border` like every other panel (rounded by default)
        let set = crate::frame::BorderSet::for_theme(&theme.border, &theme.icons);
        let tl_w = str_width(set.tl);
        let tr_w = str_width(set.tr);
        let bl_w = str_width(set.bl);
        let br_w = str_width(set.br);
        let h_w = str_width(set.h).max(1);
        let (left, right) = (format!("{} ", set.v), format!(" {}", set.v));
        let left_w = str_width(&left);
        let right_w = str_width(&right);
        let width = width.max((tl_w + tr_w).max(bl_w + br_w).max(left_w + right_w + 2)).max(8);
        let prompt_w = str_width(&opts.prompt);
        let cw = self.content_width(width, prompt_w, left_w + right_w);
        self.last_width.set(cw);

        self.sync_wrap(cw);
        let layout = self.wrap.borrow();
        let (cv, cx) = self.cursor_visual_in(&layout, cw);
        let total = self.total_visual_rows(&layout, cw);
        let max = if opts.max_height == 0 { total } else { opts.max_height.min(total) };
        let mut scroll = self.scroll.get();
        if cv < scroll {
            scroll = cv;
        }
        if cv >= scroll + max {
            scroll = cv + 1 - max;
        }
        scroll = scroll.min(total - max);
        self.scroll.set(scroll);

        let mut lines = Vec::with_capacity(max + 2);
        let mut top = Line::default();
        top.push_str(set.tl, border);
        let up_glyph = if theme.icons == "ascii" { "^" } else { "↑" };
        let above = if scroll > 0 { format!(" {up_glyph} {scroll} more ") } else { String::new() };
        let above = if str_width(&above) <= width.saturating_sub(tl_w + tr_w) { above } else { String::new() };
        top.push_str(&above, border);
        let top_fill = width.saturating_sub(tl_w + tr_w + str_width(&above));
        top.push_str(&crate::frame::repeat_to_width(set.h, h_w, top_fill), border);
        top.push_str(set.tr, border);
        lines.push(top);

        let placeholder_style = theme.fg(opts.placeholder_style.as_deref().unwrap_or("dim"));
        let token_style = theme.fg("muted");
        let sel_style = theme.style("MareSelection");
        let selection = self.selection();
        let indent = " ".repeat(prompt_w);
        for vi in scroll..scroll + max {
            let (r, seg) = self.visual_row_at(&layout, cw, vi);
            let mut l = Line::default();
            l.push_str(&left, border);
            if vi == 0 {
                l.push_str(&opts.prompt, accent);
            } else {
                l.push_str(&indent, Style::default());
            }
            let content: Line = if self.buffer_empty() {
                Line::styled(truncate_str(&opts.placeholder, cw, "…"), placeholder_style)
            } else {
                self.styled_segment(r, seg, token_style, sel_style, selection)
            };
            let mut used = content.width();
            l.spans.extend(content.spans);
            if vi == cv
                && self.col == self.lines[r].len()
                && used < cw
                && let Some(hint) = &opts.hint
            {
                let shown = truncate_str(hint, cw - used, "…");
                used += str_width(&shown);
                l.push_str(&shown, placeholder_style);
            }
            if used < cw {
                l.push_str(&" ".repeat(cw - used), Style::default());
            }
            l.push_str(&right, border);
            lines.push(l);
        }

        let mut bottom = Line::default();
        bottom.push_str(set.bl, border);
        let hidden = total - scroll - max;
        let down_glyph = if theme.icons == "ascii" { "v" } else { "↓" };
        let below = if hidden > 0 { format!(" {down_glyph} {hidden} more ") } else { String::new() };
        let below = if str_width(&below) <= width.saturating_sub(bl_w + br_w) { below } else { String::new() };
        bottom.push_str(&below, border);
        let bot_fill = width.saturating_sub(bl_w + br_w + str_width(&below));
        bottom.push_str(&crate::frame::repeat_to_width(set.h, h_w, bot_fill), border);
        bottom.push_str(set.br, border);
        lines.push(bottom);

        let crow = 1 + (cv - scroll);
        let ccol = left_w + prompt_w + cx.min(cw);
        EditorRender { lines, cursor: (crow as u16, ccol as u16) }
    }

    fn render_masked(&self, width: usize, theme: &Theme, opts: &EditorRenderOpts, mc: char) -> EditorRender {
        let mut masked_lines = Vec::with_capacity(self.lines.len());
        let map_col = |r: usize, c: usize| -> usize {
            let line = &self.lines[r];
            let c = c.min(line.len());
            line[..c].graphemes(true).count() * mc.len_utf8()
        };
        for line in &self.lines {
            let count = line.graphemes(true).count();
            let mut s = String::with_capacity(count * mc.len_utf8());
            for _ in 0..count {
                s.push(mc);
            }
            masked_lines.push(TextLine::from(s));
        }
        let temp = Self {
            lines: masked_lines,
            row: self.row,
            col: map_col(self.row, self.col),
            elements: Vec::new(),
            next_element_id: 1,
            last_submitted_elements: Vec::new(),
            paste_counter: 0,
            kill_ring: Vec::new(),
            yank_base: None,
            undo: Vec::new(),
            redo: Vec::new(),
            selection_anchor: self.selection_anchor.map(|(ar, ac)| (ar, map_col(ar, ac))),
            last_op: LastOp::Other,
            shell: self.shell,
            shell_enabled: self.shell_enabled,
            preferred_x: None,
            recalled: None,
            last_width: Cell::new(self.last_width.get()),
            scroll: Cell::new(self.scroll.get()),
            text_rev: self.text_rev,
            wrap: RefCell::new(WrapLayout::default()),
        };
        let mut inner_opts = opts.clone();
        inner_opts.mask_char = None;
        let rendered = temp.render(width, theme, &inner_opts);
        self.last_width.set(temp.last_width.get());
        self.scroll.set(temp.scroll.get());
        rendered
    }

    fn styled_segment(
        &self,
        row: usize,
        seg: Range<usize>,
        token_style: Style,
        sel_style: Style,
        selection: Option<((usize, usize), (usize, usize))>,
    ) -> Line {
        let line = &self.lines[row];
        let sel_range = selection.and_then(|((sr, sc), (er, ec))| {
            if row < sr || row > er {
                return None;
            }
            let start = if row == sr { sc } else { 0 };
            let end = if row == er { ec } else { line.len() };
            let s = start.max(seg.start);
            let e = end.min(seg.end);
            (s < e).then_some(s..e)
        });
        let tokens: Vec<Range<usize>> = self
            .token_ranges(row)
            .into_iter()
            .filter(|t| t.end > seg.start && t.start < seg.end)
            .map(|t| t.start.max(seg.start)..t.end.min(seg.end))
            .collect();
        let mut points = Vec::with_capacity(2 + tokens.len() * 2 + 2);
        points.push(seg.start);
        points.push(seg.end);
        for t in &tokens {
            points.push(t.start);
            points.push(t.end);
        }
        if let Some(s) = &sel_range {
            points.push(s.start);
            points.push(s.end);
        }
        points.sort_unstable();
        points.dedup();
        let mut out = Line::default();
        for w in points.windows(2) {
            let (a, b) = (w[0], w[1]);
            let mut st = Style::default();
            if tokens.iter().any(|t| t.start <= a && b <= t.end) {
                st = st.patch(token_style);
            }
            if sel_range.as_ref().is_some_and(|s| s.start <= a && b <= s.end) {
                st = st.patch(sel_style);
            }
            out.push_str(&line[a..b], st);
        }
        out
    }
}
