//! Layout: wrapping logical rows into visual rows (cached per row) and the cursor's place in them.

use super::*;

/// Cached wrap of one logical row: its line revision, width, theme generation, and segments.
#[derive(Default)]
struct WrapRow {
    rev: u64,
    width: usize,
    theme_gen: u64,
    segs: Vec<Range<usize>>,
    /// Width of the last segment, to decide the cursor-at-end extra row without re-measuring.
    last_w: usize,
}

#[derive(Default)]
pub(super) struct WrapLayout {
    width: usize,
    text_rev: u64,
    theme_gen: u64,
    rows: Vec<WrapRow>,
    /// Visual row start index for each logical line `0..=lines.len()`, before cursor-at-end extra row.
    starts: Vec<usize>,
    total_base: usize,
}

impl Editor {
    /// Wrap one line at content width `w`: segments and the last segment's width.
    fn wrap_line(line: &str, w: usize) -> (Vec<Range<usize>>, usize) {
        let w = w.max(1);
        let mut segs = Vec::new();
        let mut start = 0;
        let mut cur_w = 0;
        for (i, g) in line.grapheme_indices(true) {
            let gw = grapheme_width(g);
            if cur_w + gw > w && i > start {
                segs.push(start..i);
                start = i;
                cur_w = 0;
            }
            cur_w += gw;
        }
        segs.push(start..line.len());
        (segs, cur_w)
    }

    pub(super) fn sync_wrap(&self, w: usize) {
        let tgen = mare_shared::style::theme_gen();
        let mut layout = self.wrap.borrow_mut();
        if layout.width == w && layout.text_rev == self.text_rev && layout.theme_gen == tgen {
            return;
        }
        let n = self.lines.len();
        layout.rows.resize_with(n, WrapRow::default);
        layout.starts.clear();
        layout.starts.reserve(n + 1);
        let mut acc = 0usize;
        for (r, line) in self.lines.iter().enumerate() {
            layout.starts.push(acc);
            let c = &mut layout.rows[r];
            if c.width != w || c.rev != line.rev() || c.theme_gen != tgen || c.segs.is_empty() {
                let (segs, last_w) = Self::wrap_line(line, w);
                c.rev = line.rev();
                c.width = w;
                c.theme_gen = tgen;
                c.segs = segs;
                c.last_w = last_w;
            }
            acc += c.segs.len();
        }
        layout.starts.push(acc);
        layout.total_base = acc;
        layout.width = w;
        layout.text_rev = self.text_rev;
        layout.theme_gen = tgen;
    }

    fn cursor_extra_row(&self, layout: &WrapLayout, w: usize) -> bool {
        let line = &self.lines[self.row];
        self.col == line.len() && layout.rows[self.row].last_w >= w.max(1) && !line.is_empty()
    }

    pub(super) fn total_visual_rows(&self, layout: &WrapLayout, w: usize) -> usize {
        layout.total_base + usize::from(self.cursor_extra_row(layout, w))
    }

    pub(super) fn cursor_visual_in(&self, layout: &WrapLayout, w: usize) -> (usize, usize) {
        let row_segs = &layout.rows[self.row].segs;
        if self.cursor_extra_row(layout, w) {
            return (layout.starts[self.row] + row_segs.len(), 0);
        }
        let seg_idx = row_segs.partition_point(|s| s.start <= self.col).saturating_sub(1);
        let seg = &row_segs[seg_idx];
        let x = str_width(&self.lines[self.row][seg.start..self.col.max(seg.start)]);
        (layout.starts[self.row] + seg_idx, x)
    }

    pub(super) fn visual_row_at(&self, layout: &WrapLayout, w: usize, vi: usize) -> (usize, Range<usize>) {
        let extra = self.cursor_extra_row(layout, w);
        let base_vi = if extra {
            let extra_vi = layout.starts[self.row + 1];
            if vi == extra_vi {
                let end = self.lines[self.row].len();
                return (self.row, end..end);
            } else if vi > extra_vi {
                vi - 1
            } else {
                vi
            }
        } else {
            vi
        };
        let r = layout.starts.partition_point(|&s| s <= base_vi).saturating_sub(1);
        let seg_idx = base_vi - layout.starts[r];
        (r, layout.rows[r].segs[seg_idx].clone())
    }

    pub(super) fn content_width(&self, total: usize, prompt_w: usize, border_w: usize) -> usize {
        total.saturating_sub(border_w + prompt_w).max(1)
    }
}
