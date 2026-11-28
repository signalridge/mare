//! Interface shell primitives: border charsets, boxes with a
//! title embedded in the top border and a footer in the bottom one, a thin scrollbar on the right
//! border, and list virtual scrolling. The *look* (styles, which border is focused, what the
//! title says) is decided by Lua components; this module only knows how to draw.

use super::style::*;

/// Border characters: tl, tr, bl, br, horizontal, vertical.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BorderSet {
    pub tl: &'static str,
    pub tr: &'static str,
    pub bl: &'static str,
    pub br: &'static str,
    pub h: &'static str,
    pub v: &'static str,
}

impl BorderSet {
    pub fn named(name: &str) -> Self {
        match name {
            "single" | "plain" => Self { tl: "┌", tr: "┐", bl: "└", br: "┘", h: "─", v: "│" },
            "double" => Self { tl: "╔", tr: "╗", bl: "╚", br: "╝", h: "═", v: "║" },
            "thick" => Self { tl: "┏", tr: "┓", bl: "┗", br: "┛", h: "━", v: "┃" },
            "hidden" | "none" => Self { tl: " ", tr: " ", bl: " ", br: " ", h: " ", v: " " },
            _ => Self { tl: "╭", tr: "╮", bl: "╰", br: "╯", h: "─", v: "│" },
        }
    }

    pub fn ascii() -> Self {
        Self { tl: "+", tr: "+", bl: "+", br: "+", h: "-", v: "|" }
    }

    pub fn for_theme(name: &str, icons: &str) -> Self {
        if icons == "ascii" && !matches!(name, "hidden" | "none") { Self::ascii() } else { Self::named(name) }
    }
}

pub(crate) fn repeat_to_width(unit: &str, unit_w: usize, target_w: usize) -> String {
    if target_w == 0 {
        return String::new();
    }
    let uw = unit_w.max(1);
    let mut s = unit.repeat(target_w / uw);
    for _ in 0..(target_w % uw) {
        s.push(' ');
    }
    s
}

/// Scrollbar position for a box: `top` = first visible row, `visible` rows of `total`.
#[derive(Clone, Copy, Debug)]
pub struct Scroll {
    pub top: usize,
    pub visible: usize,
    pub total: usize,
}

/// Options for [`boxed`].
pub struct BoxOpts {
    pub border: BorderSet,
    pub border_style: Style,
    /// spans embedded in the top border after `─ ` (already styled)
    pub title: Vec<Span>,
    /// spans embedded at the right end of the top border (`Sort: Updated`): shortened, then
    /// left out, when the title leaves too little room — the title always wins
    pub title_right: Vec<Span>,
    /// text embedded at the right of the bottom border (`3 of 42`)
    pub footer: Option<String>,
    pub footer_style: Style,
    pub scroll: Option<Scroll>,
    pub scrollbar_style: Style,
    pub scrollbar_char: &'static str,
}

impl BoxOpts {
    pub fn new(border: BorderSet, border_style: Style) -> Self {
        Self {
            border,
            border_style,
            title: vec![],
            title_right: vec![],
            footer: None,
            footer_style: border_style,
            scroll: None,
            scrollbar_style: border_style,
            scrollbar_char: "┃",
        }
    }

    pub fn for_theme(theme: &Theme, border_style: Style) -> Self {
        let mut o = Self::new(BorderSet::for_theme(&theme.border, &theme.icons), border_style);
        if theme.icons == "ascii" {
            o.scrollbar_char = "|";
        }
        o
    }
}

/// Which rows of a `visible`-row track hold the scrollbar thumb.
pub fn thumb(s: Scroll) -> std::ops::Range<usize> {
    if s.total <= s.visible || s.visible == 0 {
        return 0..0;
    }
    let len = ((s.visible * s.visible) / s.total).clamp(1, s.visible);
    let max_top = s.total - s.visible;
    let pos = ((s.visible - len) * s.top.min(max_top)).div_ceil(max_top.max(1));
    pos..pos + len
}

/// Draw `content` (already ≤ inner width) inside a box `width` columns wide.
pub fn boxed(content: &[Line], width: usize, o: &BoxOpts) -> Vec<Line> {
    let b = o.border;
    let bs = o.border_style;
    let tl_w = str_width(b.tl);
    let tr_w = str_width(b.tr);
    let bl_w = str_width(b.bl);
    let br_w = str_width(b.br);
    let h_w = str_width(b.h).max(1);
    let vw = str_width(b.v);
    let bar_w = if o.scroll.is_some() { str_width(o.scrollbar_char) } else { 0 };
    let right_v_w = vw.max(bar_w);
    let min_w = (tl_w + tr_w).max(bl_w + br_w).max(vw + right_v_w + 3).max(6);
    let width = width.max(min_w);
    let inner = width.saturating_sub(vw + 1 + 1 + right_v_w);
    let mut out = Vec::with_capacity(content.len() + 2);
    // top: ╭─ Title ─────╮
    let tail_chrome = h_w + tr_w;
    let mut top = Line::new(vec![Span::styled(format!("{}{}", b.tl, b.h), bs)]);
    let mut used = tl_w + h_w;
    if !o.title.is_empty() && width > used + 2 + tail_chrome {
        let max_title = width.saturating_sub(used + 2 + tail_chrome);
        let t = truncate_line(&Line::new(o.title.clone()), max_title);
        top.push(Span::raw(" "));
        used += 1 + t.width();
        top.spans.extend(t.spans);
        top.push(Span::raw(" "));
        used += 1;
    }
    // ` right ─╮` after at least one fill character; under RIGHT_MIN columns it isn't worth it
    const RIGHT_MIN: usize = 8;
    let room = width.saturating_sub(used + h_w + 2 + tail_chrome);
    if o.title_right.is_empty() || room < RIGHT_MIN {
        let fill = width.saturating_sub(used + tr_w);
        top.push(Span::styled(format!("{}{}", repeat_to_width(b.h, h_w, fill), b.tr), bs));
    } else {
        let right = truncate_line(&Line::new(o.title_right.clone()), room);
        let fill = width.saturating_sub(used + 1 + right.width() + 1 + tail_chrome);
        top.push(Span::styled(repeat_to_width(b.h, h_w, fill), bs));
        top.push(Span::raw(" "));
        top.spans.extend(right.spans);
        top.push(Span::raw(" "));
        top.push(Span::styled(format!("{}{}", b.h, b.tr), bs));
    }
    out.push(top);
    let bar = o.scroll.map(thumb).unwrap_or(0..0);
    for (i, l) in content.iter().enumerate() {
        let l = truncate_line(l, inner);
        let has_bar = bar.contains(&i);
        let rw = if has_bar { bar_w } else { vw };
        let pad = inner.saturating_sub(l.width()) + right_v_w.saturating_sub(rw);
        let mut row = Line::new(vec![Span::styled(format!("{} ", b.v), bs)]);
        let fill = l.fill_bg;
        row.spans.extend(l.spans);
        match fill {
            Some(bg) => row.push(Span::styled(" ".repeat(pad + 1), Style::new().bg(bg))),
            None => row.push(Span::raw(" ".repeat(pad + 1))),
        }
        if has_bar {
            row.push(Span::styled(o.scrollbar_char, o.scrollbar_style));
        } else {
            row.push(Span::styled(b.v, bs));
        }
        out.push(row);
    }
    // bottom: ╰──────── 3 of 42 ─╯
    let mut bottom = Line::new(vec![Span::styled(b.bl, bs)]);
    match &o.footer {
        Some(f) if str_width(f) + 2 + bl_w + h_w + h_w + br_w <= width => {
            let fw = str_width(f);
            let fill = width.saturating_sub(bl_w + fw + 2 + h_w + br_w);
            bottom.push(Span::styled(repeat_to_width(b.h, h_w, fill), bs));
            bottom.push(Span::styled(format!(" {f} "), o.footer_style));
            bottom.push(Span::styled(format!("{}{}", b.h, b.br), bs));
        }
        _ => {
            let fill = width.saturating_sub(bl_w + br_w);
            bottom.push(Span::styled(format!("{}{}", repeat_to_width(b.h, h_w, fill), b.br), bs));
        }
    }
    out.push(bottom);
    out
}

/// First visible row of a list window of `height` rows keeping `selected` in view.
pub fn window_start(selected: usize, total: usize, height: usize, prev: usize) -> usize {
    if total <= height {
        return 0;
    }
    let mut start = prev.min(total - height);
    if selected < start {
        start = selected;
    } else if selected >= start + height {
        start = selected + 1 - height;
    }
    start
}

/// Render list rows (virtual scrolling: only the visible window is materialized). The selected
/// row gets a full-width background when focused, bold otherwise (lazygit). Returns the rows and
/// the scroll position for the box scrollbar.
pub fn list_rows(
    items: &[Line],
    selected: Option<usize>,
    focused: bool,
    height: usize,
    width: usize,
    theme: &Theme,
) -> (Vec<Line>, Scroll) {
    let height = height.max(1);
    let sel = selected.unwrap_or(0);
    let start = window_start(sel, items.len(), height, 0);
    let mut rows = Vec::with_capacity(height.min(items.len()));
    for (i, it) in items.iter().enumerate().skip(start).take(height) {
        rows.push(list_row(it, selected == Some(i), focused, width, theme));
    }
    (rows, Scroll { top: start, visible: height, total: items.len() })
}

/// One list row, truncated to `width`; a selected one gets `MareSelection` when focused,
/// `MareSelectionInactive` otherwise.
pub fn list_row(it: &Line, selected: bool, focused: bool, width: usize, theme: &Theme) -> Line {
    let mut l = truncate_line(it, width);
    if selected {
        if focused {
            let st = theme.style("MareSelection");
            for s in &mut l.spans {
                if s.style.bg.is_none() {
                    s.style.bg = st.bg;
                }
                if st.bold {
                    s.style.bold = true;
                }
                // NO_COLOR: the selection is reverse video instead of a background
                if st.reverse {
                    s.style.reverse = true;
                }
            }
            l.fill_bg = st.bg;
        } else {
            let st = theme.style("MareSelectionInactive");
            for s in &mut l.spans {
                if s.style.fg.is_none() {
                    s.style.fg = st.fg;
                }
                if s.style.bg.is_none() {
                    s.style.bg = st.bg;
                }
                if st.bold {
                    s.style.bold = true;
                }
                if st.dim {
                    s.style.dim = true;
                }
                if st.italic {
                    s.style.italic = true;
                }
                if st.underline {
                    s.style.underline = true;
                }
                if st.reverse {
                    s.style.reverse = true;
                }
            }
            l.fill_bg = st.bg;
        }
    }
    l
}

/// [`list_rows`] for items of one or more rows each (a select's cards): the window is `height`
/// rows, starts at an item's first row and shows the whole selected item (its top, when it is
/// taller than the window); every row of the selected item is highlighted. The scroll position
/// is in rows. With one row per item this is exactly `list_rows`.
pub fn list_items(
    items: &[Vec<Line>],
    selected: Option<usize>,
    focused: bool,
    height: usize,
    width: usize,
    theme: &Theme,
) -> (Vec<Line>, Scroll) {
    let height = height.max(1);
    // the first row of each item, and one past the last
    let mut starts = Vec::with_capacity(items.len() + 1);
    let mut total = 0;
    for it in items {
        starts.push(total);
        total += it.len().max(1);
    }
    starts.push(total);
    let sel = selected.unwrap_or(0).min(items.len().saturating_sub(1));
    let start = if total <= height || items.is_empty() {
        0
    } else {
        // the earliest item start that keeps the selected item's end in view, never past its top
        let need = starts[sel + 1].saturating_sub(height);
        let top = starts.iter().copied().find(|&s| s >= need).unwrap_or(starts[sel]);
        top.min(starts[sel]).min(total - height)
    };
    // only the rows in the window are materialized
    let empty = Line::empty();
    let rows = items
        .iter()
        .enumerate()
        .flat_map(|(i, it)| {
            let lines: &[Line] = if it.is_empty() { std::slice::from_ref(&empty) } else { it };
            lines.iter().map(move |l| (l, i))
        })
        .skip(start)
        .take(height)
        .map(|(l, i)| list_row(l, selected == Some(i), focused, width, theme))
        .collect();
    (rows, Scroll { top: start, visible: height, total })
}

/// Side-by-side (or stacked, below `stack_below` columns) composition of two boxes.
pub fn split(left: Vec<Line>, right: Vec<Line>, gap: usize) -> Vec<Line> {
    let lw = left.iter().map(Line::width).max().unwrap_or(0);
    let n = left.len().max(right.len());
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut l = left.get(i).cloned().unwrap_or_else(Line::empty);
        let pad = lw.saturating_sub(l.width()) + gap;
        l.push(Span::raw(" ".repeat(pad)));
        if let Some(r) = right.get(i) {
            l.spans.extend(r.spans.iter().cloned());
        }
        out.push(l);
    }
    out
}

/// The character positions of `text` that `pattern` matches (fuzzy, smart case), sorted; None
/// when it doesn't match.
fn match_indices(text: &str, pattern: &str) -> Option<Vec<u32>> {
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};
    // one matcher per thread, reused: `Matcher::new` allocates its scoring matrices, and a
    // popup highlights every visible row on every frame
    thread_local! {
        static MATCHER: std::cell::RefCell<Matcher> = std::cell::RefCell::new(Matcher::new(Config::DEFAULT));
    }
    let pat = Pattern::parse(pattern, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut idx: Vec<u32> = Vec::new();
    let found = MATCHER.with(|m| pat.indices(Utf32Str::new(text, &mut buf), &mut m.borrow_mut(), &mut idx).is_some());
    if !found {
        return None;
    }
    idx.sort_unstable();
    idx.dedup();
    Some(idx)
}

/// Highlight the characters of `text` matched by `pattern` (fuzzy, smart case) with `hl`.
pub fn highlight_matches(text: &str, pattern: &str, base: Style, hl: Style) -> Vec<Span> {
    if pattern.trim().is_empty() {
        return vec![Span::styled(text, base)];
    }
    let Some(idx) = match_indices(text, pattern) else {
        return vec![Span::styled(text, base)];
    };
    let mut out: Vec<Span> = vec![];
    let mut cur = String::new();
    let mut cur_hl = false;
    for (i, ch) in text.chars().enumerate() {
        let h = idx.binary_search(&(i as u32)).is_ok();
        if h != cur_hl && !cur.is_empty() {
            out.push(Span::styled(std::mem::take(&mut cur), if cur_hl { hl } else { base }));
        }
        cur_hl = h;
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(Span::styled(cur, if cur_hl { hl } else { base }));
    }
    out
}

/// [`highlight_matches`] on an already styled line (a multi-line select item's row): matched
/// characters take `hl`, the rest keep their style.
pub fn highlight_line(line: &Line, pattern: &str, hl: Style) -> Line {
    if pattern.trim().is_empty() {
        return line.clone();
    }
    let Some(idx) = match_indices(&line.plain(), pattern) else { return line.clone() };
    let mut out = Line { spans: vec![], fill_bg: line.fill_bg, raw: None };
    let mut i = 0u32;
    for span in &line.spans {
        let mut cur = String::new();
        let mut cur_hl = false;
        for ch in span.text.chars() {
            let h = idx.binary_search(&i).is_ok();
            if h != cur_hl && !cur.is_empty() {
                out.push(Span::styled(std::mem::take(&mut cur), if cur_hl { hl } else { span.style }));
            }
            cur_hl = h;
            cur.push(ch);
            i += 1;
        }
        if !cur.is_empty() {
            out.push(Span::styled(cur, if cur_hl { hl } else { span.style }));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn box_title_footer_scrollbar() {
        let theme = Theme::default();
        let items: Vec<Line> = (0..42).map(|i| Line::raw(format!("item {i}"))).collect();
        let (rows, scroll) = list_rows(&items, Some(2), true, 5, 20, &theme);
        let mut o = BoxOpts::new(BorderSet::named("rounded"), theme.style("MareBorderActive"));
        o.title = vec![Span::raw("Agents")];
        o.footer = Some("3 of 42".into());
        o.scroll = Some(scroll);
        let b = boxed(&rows, 24, &o);
        assert_eq!(b.len(), 7);
        assert!(text(&b[0]).starts_with("╭─ Agents ─"), "{}", text(&b[0]));
        assert!(text(&b[6]).ends_with(" 3 of 42 ─╯"), "{}", text(&b[6]));
        assert!(b.iter().all(|l| l.width() == 24), "{:?}", b.iter().map(Line::width).collect::<Vec<_>>());
        // selected row has the selection background; the scrollbar thumb sits on the right border
        assert!(rows[2].fill_bg.is_some());
        assert!(b[1..6].iter().any(|l| text(l).ends_with('┃')));
    }

    /// Items of several rows scroll by whole items and keep the selected one in full view;
    /// one row per item is `list_rows` exactly.
    #[test]
    fn multi_row_items_scroll_by_item_and_highlight_whole() {
        let theme = Theme::default();
        let card = |i: usize| vec![Line::raw(format!("title {i}")), Line::raw(format!("detail {i}"))];
        let cards: Vec<Vec<Line>> = (0..10).map(card).collect();
        // 3 cards = 6 rows in a 5-row window: the window starts at a card's first row
        let (rows, scroll) = list_items(&cards, Some(2), true, 5, 20, &theme);
        assert_eq!(
            rows.iter().map(text).collect::<Vec<_>>(),
            ["title 1", "detail 1", "title 2", "detail 2", "title 3"]
        );
        assert_eq!((scroll.top, scroll.total), (2, 20));
        assert!(rows[2].fill_bg.is_some() && rows[3].fill_bg.is_some(), "both rows of the selected card");
        assert!(rows[0].fill_bg.is_none() && rows[4].fill_bg.is_none());
        // the last card: the window ends with it; a card taller than the window shows its top
        let (rows, _) = list_items(&cards, Some(9), true, 5, 20, &theme);
        assert_eq!(text(&rows[4]), "detail 9");
        let tall = vec![vec![Line::raw("a")], (0..8).map(|i| Line::raw(format!("t{i}"))).collect()];
        let (rows, _) = list_items(&tall, Some(1), true, 5, 20, &theme);
        assert_eq!(text(&rows[0]), "t0");
        // one row each: the same rows and scroll as list_rows
        let single: Vec<Line> = (0..42).map(|i| Line::raw(format!("item {i}"))).collect();
        let wrapped: Vec<Vec<Line>> = single.iter().map(|l| vec![l.clone()]).collect();
        for sel in [0, 4, 5, 17, 41] {
            let (a, sa) = list_rows(&single, Some(sel), true, 5, 20, &theme);
            let (b, sb) = list_items(&wrapped, Some(sel), true, 5, 20, &theme);
            assert_eq!(a, b, "selected {sel}");
            assert_eq!((sa.top, sa.visible, sa.total), (sb.top, sb.visible, sb.total));
        }
        // the search highlight on a styled row keeps the other characters' styles
        let styled = Line::new(vec![Span::styled("ab", Style::new().italic()), Span::raw("cd")]);
        let h = highlight_line(&styled, "bc", Style::new().bold());
        assert_eq!(text(&h), "abcd");
        let bold: String = h.spans.iter().filter(|s| s.style.bold).map(|s| s.text.as_str()).collect();
        assert_eq!(bold, "bc");
        assert!(h.spans[0].style.italic && !h.spans[0].style.bold, "{:?}", h.spans);
    }

    /// `title_right` sits at the right end of the top border; a narrower box shortens it,
    /// then leaves it out, and the title is never cut for it.
    #[test]
    fn title_right_gives_way_to_the_title() {
        let mut o = BoxOpts::new(BorderSet::named("rounded"), Style::new());
        o.title = vec![Span::raw("Resume session")];
        o.title_right = vec![Span::raw("Sort: Updated · Filter: Cwd")];
        let top = |o: &BoxOpts, w: usize| text(&boxed(&[], w, o)[0]);
        assert_eq!(top(&o, 60), "╭─ Resume session ─────────── Sort: Updated · Filter: Cwd ─╮");
        assert_eq!(top(&o, 40), "╭─ Resume session ─ Sort: Updated · … ─╮");
        assert_eq!(top(&o, 26), "╭─ Resume session ───────╮", "under 8 columns left: no right part");
        assert_eq!(top(&o, 14), "╭─ Resume … ─╮", "the title is cut as before");
        for w in [14, 26, 40, 60] {
            assert_eq!(boxed(&[], w, &o)[0].width(), w);
        }
        o.title_right.clear();
        assert_eq!(top(&o, 30), "╭─ Resume session ───────────╮", "none: the border as before");
    }

    #[test]
    fn window_follows_selection() {
        assert_eq!(window_start(0, 3, 5, 0), 0);
        assert_eq!(window_start(9, 42, 5, 0), 5);
        assert_eq!(window_start(6, 42, 5, 5), 5);
        assert_eq!(window_start(4, 42, 5, 5), 4);
    }

    #[test]
    fn block_list_node_and_split_stacking() {
        use mare_shared::ui::Node;
        let theme = Theme::default();
        let list = Node::List {
            items: (0..30).map(|i| Line::raw(format!("row {i}"))).collect(),
            selected: Some(20),
            focused: false,
            height: 4,
        };
        let block = Node::Block {
            title: vec![Span::raw("Agents")],
            footer: Some("21 of 30".into()),
            border_style: "MareBorder".into(),
            border: Some("single".into()),
            child: Box::new(list),
        };
        let out = crate::render::render_node(&block, 30, &theme, 0);
        assert_eq!(out.len(), 6, "4 visible rows + borders");
        assert!(text(&out[0]).starts_with("┌─ Agents"));
        assert!(out[1..5].iter().any(|l| text(l).contains("row 20")), "selection kept in view");
        // unfocused selection: bold, no background
        let sel = out.iter().find(|l| text(l).contains("row 20")).unwrap();
        assert!(sel.spans.iter().any(|s| s.style.bold) && sel.fill_bg.is_none());
        let split = Node::Split {
            left: Box::new(Node::Line(Line::raw("L"))),
            right: Box::new(Node::Line(Line::raw("R"))),
            ratio: 0.4,
            stack_below: 84,
        };
        assert_eq!(crate::render::render_node(&split, 60, &theme, 0).len(), 2, "narrow: stacked");
        let wide = crate::render::render_node(&split, 100, &theme, 0);
        assert_eq!(wide.len(), 1);
        assert!(text(&wide[0]).starts_with('L') && text(&wide[0]).ends_with('R'));
    }

    #[test]
    fn border_sets_and_matches() {
        assert_eq!(BorderSet::named("double").tl, "╔");
        assert_eq!(BorderSet::named("thick").tl, "┏");
        assert_eq!(BorderSet::named("hidden").tl, " ");
        assert_eq!(BorderSet::named("none").tl, " ");
        assert_eq!(BorderSet::named("whatever").tl, "╭");
        let s = highlight_matches("session.lua", "sl", Style::new(), Style::new().bold());
        assert!(s.iter().any(|x| x.style.bold));
        assert_eq!(s.iter().map(|x| x.text.as_str()).collect::<String>(), "session.lua");

        // Blank or non-matching pattern returns unhighlighted base span.
        let blank = highlight_matches("session.lua", "   ", Style::new().dim(), Style::new().bold());
        assert_eq!(blank.len(), 1);
        assert!(blank[0].style.dim && !blank[0].style.bold);
        let miss = highlight_matches("session.lua", "zzz", Style::new().dim(), Style::new().bold());
        assert_eq!(miss.len(), 1);
        assert!(miss[0].style.dim && !miss[0].style.bold);

        // ASCII theme switches scrollbar char to "|" and inactive selection forwards dim/underline/reverse.
        let mut theme = Theme { icons: "ascii".into(), ..Default::default() };
        let o = BoxOpts::for_theme(&theme, Style::new());
        assert_eq!(o.scrollbar_char, "|");
        theme.set_style("MareSelectionInactive", Style::new().dim().underline().reverse());
        let row = list_row(&Line::raw("item"), true, false, 12, &theme);
        assert!(row.spans[0].style.dim && row.spans[0].style.underline && row.spans[0].style.reverse);
    }

    #[test]
    fn inactive_selection_uses_theme_style() {
        let mut theme = Theme::default();
        let bg = Color::Rgb(12, 24, 36);
        let fg = Color::Rgb(200, 210, 220);
        theme.set_style("MareSelectionInactive", Style::new().fg(fg).bg(bg).italic());
        let row = list_row(&Line::raw("item"), true, false, 12, &theme);
        assert_eq!(row.fill_bg, Some(bg), "inactive selection should apply theme fill_bg");
        assert_eq!(row.spans[0].style.fg, Some(fg), "inactive selection should apply theme fg");
        assert_eq!(row.spans[0].style.bg, Some(bg), "inactive selection should apply theme bg");
        assert!(row.spans[0].style.italic, "inactive selection should apply theme italic");
        assert!(!row.spans[0].style.bold, "inactive selection should not hardcode bold when theme omits it");
    }
}
