//! Tables: aligned columns that narrow to fit, degrading to key/value lines and then source.

use super::*;

/// Column gap in tables.
const TABLE_GAP: usize = 2;

impl Renderer<'_> {
    pub(super) fn render_table(&mut self, t: TableState) {
        let ncols = t.rows.iter().map(Vec::len).max().unwrap_or(0);
        if ncols == 0 {
            return;
        }
        let avail = self.avail();
        let gaps = TABLE_GAP * (ncols - 1);
        let mut widths = vec![1usize; ncols];
        let mut mins = vec![1usize; ncols];
        for row in &t.rows {
            for (i, c) in row.iter().enumerate() {
                widths[i] = widths[i].max(c.width());
                // narrowest a column may get: its longest word, at least 3
                let word = c.plain().split_whitespace().map(str_width).max().unwrap_or(0);
                mins[i] = mins[i].max(word.clamp(3, 16).min(widths[i]));
            }
        }
        if mins.iter().sum::<usize>() + gaps > avail {
            // too narrow for columns: each row as "key: value" lines, then the source
            if avail >= 16 && t.header_rows > 0 {
                return self.render_table_kv(&t);
            }
            let st = self.st("md.html");
            for l in t.source.trim_end().lines() {
                for w in wrap_line(&Line::styled(l, st), avail) {
                    self.emit(w, true);
                }
            }
            return;
        }
        // narrow the widest columns until the table fits (cells then wrap)
        while widths.iter().sum::<usize>() + gaps > avail {
            let Some((idx, _)) = widths.iter().enumerate().filter(|(i, w)| **w > mins[*i]).max_by_key(|(_, w)| **w)
            else {
                break;
            };
            widths[idx] -= 1;
        }
        let header_st = self.st("md.table_header");
        for (ri, row) in t.rows.iter().enumerate() {
            let header = ri < t.header_rows;
            let cells: Vec<Vec<Line>> = (0..ncols)
                .map(|ci| {
                    let cell = row.get(ci).cloned().unwrap_or_default();
                    let cell = if header { cell.base_style(header_st) } else { cell };
                    wrap_line(&cell, widths[ci])
                        .into_iter()
                        .map(|l| if l.width() > widths[ci] { truncate_line(&l, widths[ci]) } else { l })
                        .collect()
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
            for h in 0..height {
                let mut line = Line::empty();
                for ci in 0..ncols {
                    if ci > 0 {
                        line.push(Span::raw(" ".repeat(TABLE_GAP)));
                    }
                    let content = cells[ci].get(h).cloned().unwrap_or_default();
                    let pad = widths[ci].saturating_sub(content.width());
                    let align = t.aligns.get(ci).copied().unwrap_or(Alignment::None);
                    let (l, r) = match align {
                        Alignment::Right => (pad, 0),
                        Alignment::Center => (pad / 2, pad - pad / 2),
                        _ => (0, pad),
                    };
                    if l > 0 {
                        line.push(Span::raw(" ".repeat(l)));
                    }
                    for s in content.spans {
                        line.push(s);
                    }
                    if r > 0 && ci + 1 < ncols {
                        line.push(Span::raw(" ".repeat(r)));
                    }
                }
                self.emit(line, true);
            }
            if header && ri + 1 == t.header_rows {
                let total = widths.iter().sum::<usize>() + gaps;
                let g = self.glyph("table_rule");
                let line = Line::styled(g.repeat(total / str_width(&g).max(1)), self.st("md.table_rule"));
                self.emit(line, true);
            }
        }
    }

    /// Render every body row as `header: value` lines, rows apart.
    fn render_table_kv(&mut self, t: &TableState) {
        let header = t.rows.first().cloned().unwrap_or_default();
        let key_st = self.st("md.table_header");
        let avail = self.avail();
        for (ri, row) in t.rows.iter().enumerate().skip(t.header_rows) {
            if ri > t.header_rows {
                self.emit_blank();
            }
            for (ci, cell) in row.iter().enumerate() {
                let key = header.get(ci).map(Line::plain).unwrap_or_default();
                let mut l = Line::new(vec![Span::styled(format!("{key}: "), key_st)]);
                let indent = vec![Span::raw("  ")];
                for s in &cell.spans {
                    l.push(s.clone());
                }
                for w in wrap_line_indent(&l, avail, &indent) {
                    self.emit(w, true);
                }
            }
        }
    }
}
