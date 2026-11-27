//! Node → lines layout. Called every frame for cached nodes; cheap — the expensive layouts
//! (Markdown, wrapping a long paragraph, a tool-output preview cut from a long output) are cached
//! by content in bounded LRUs.

use super::style::*;
use super::{diff, frame, highlight, markdown};
use mare_shared::ui::Node;

// Keep in sync with the Lua-node limit in mare-binding/src/ui_node.rs.
const MAX_GAP_LINES: usize = 1_000;

/// The animation frame that means "no motion": spinners draw
/// a static marker instead of cycling.
pub const STATIC_FRAME: usize = usize::MAX;

/// The animation frame that means "pick frame by wall-clock [`anim_millis`]" (live TUI),
/// so `Node::Spinner` cannot be starved by `ui_rx` traffic while snapshot callers passing
/// explicit indices (`0..`) stay deterministic.
pub const WALL_CLOCK_FRAME: usize = usize::MAX - 1;

const SPINNER_STEP_MS: u64 = 80;

thread_local! {
    static CLOCK: std::cell::Cell<fn() -> std::time::Instant> =
        const { std::cell::Cell::new(std::time::Instant::now) };
    static ANIM_T0: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

/// Current monotonic time on this thread's UI/animation clock.
pub fn clock_now() -> std::time::Instant {
    CLOCK.with(|c| (c.get())())
}

/// Override the thread-local UI/animation clock (e.g. `tokio::time::Instant::now` so
/// `tokio::time::pause()` drives `anim_millis()` and UI timers deterministically).
pub fn set_clock(clock: fn() -> std::time::Instant) {
    CLOCK.with(|c| c.set(clock));
    ANIM_T0.with(|t| t.set(Some(clock())));
}

/// Milliseconds on the animation clock ([`Node::Frames`] and live [`Node::Spinner`] pick their frame by it).
pub fn anim_millis() -> u64 {
    let now = clock_now();
    let t0 = ANIM_T0.with(|cell| {
        if let Some(t) = cell.get() {
            t
        } else {
            cell.set(Some(now));
            now
        }
    });
    now.saturating_duration_since(t0).as_millis() as u64
}

/// The spinner glyph for an animation frame: the theme's frames (`mare.o.spinner`), or its
/// still marker for [`STATIC_FRAME`]. Under [`WALL_CLOCK_FRAME`] the frame index is derived
/// from [`anim_millis`] (matching [`Node::Frames`]). Data read from the theme; no Lua on the frame path.
pub fn spinner_glyph(theme: &Theme, frame: usize) -> &str {
    if frame == STATIC_FRAME {
        theme.spinner_glyph(0, true)
    } else if frame == WALL_CLOCK_FRAME {
        theme.spinner_glyph((anim_millis() / SPINNER_STEP_MS) as usize, false)
    } else {
        theme.spinner_glyph(frame, false)
    }
}

pub fn render_node(node: &Node, width: usize, theme: &Theme, frame: usize) -> Vec<Line> {
    let width = width.max(4);
    match node {
        Node::Empty => vec![],
        Node::Line(l) => vec![truncate_line(&l.resolve(theme), width)],
        Node::Paragraph { line, indent } => {
            let wrap = || {
                let line = line.resolve(theme);
                let indent: Vec<Span> = indent.iter().map(|s| s.resolve(theme)).collect();
                let mut out = vec![];
                // honor embedded newlines
                for part in split_line(&line) {
                    out.extend(wrap_line_indent(&part, width, &indent));
                }
                out
            };
            // a paragraph that fits needs no work; one that wraps is cached by (content, width, theme_gen)
            if line.width() <= width && !line.spans.iter().any(|s| s.text.contains('\n')) {
                return vec![line.resolve(theme)];
            }
            cache::WRAP.get_or_insert(cache::key(&(line, indent), width), wrap)
        }
        Node::Lines(items) => {
            let mut lines = vec![];
            for item in items {
                let _offset = super::image::row_offset(lines.len());
                lines.extend(render_node(item, width, theme, frame));
            }
            lines
        }
        Node::Markdown(text) => {
            if mare_shared::markdown::has_images(text) {
                markdown::render_markdown(text, width, theme)
            } else {
                cache::MARKDOWN.get_or_insert(cache::key(&(text, super::image::generation()), width), || {
                    markdown::render_markdown(text, width, theme)
                })
            }
        }
        Node::PresentedMarkdown { text, blocks } => markdown::render_presented(text, width, theme, blocks, frame),
        Node::Code { code, lang } => {
            highlight::highlight(code, lang, theme).into_iter().flat_map(|l| wrap_line(&l, width)).collect()
        }
        Node::Diff { old, new, path } => diff::render_diff(old, new, path.as_deref(), width, theme),
        Node::Unified { diff, path, lines } => diff::render_unified(diff, path.as_deref(), *lines, width, theme),
        Node::Indent { first, rest, child } => {
            let first: Vec<Span> = first.iter().map(|s| s.resolve(theme)).collect();
            let rest: Vec<Span> = rest.iter().map(|s| s.resolve(theme)).collect();
            let fw: usize = first.iter().map(Span::width).sum();
            let rw: usize = rest.iter().map(Span::width).sum();
            let inner = render_node(child, width.saturating_sub(fw.max(rw)).max(4), theme, frame);
            inner
                .into_iter()
                .enumerate()
                .map(|(i, l)| {
                    let fill = l.fill_bg;
                    let mut nl = l.prefixed(if i == 0 { first.clone() } else { rest.clone() });
                    nl.fill_bg = fill;
                    nl
                })
                .collect()
        }
        Node::Gap(n) => vec![Line::empty(); (*n).min(MAX_GAP_LINES)],
        Node::Image { image_id, resource, fallback, path, bytes, max_rows, max_cols } => {
            let src = match (resource, path, bytes) {
                (Some(resource), _, _) => super::image::Source::Resource(resource.clone()),
                (_, _, Some(b)) => super::image::Source::Bytes(b.clone()),
                (_, Some(p), None) => super::image::Source::Path(p.clone()),
                (None, None, None) => return vec![],
            };
            if let Some(fallback) = fallback
                && (super::image::protocol() == super::image::Protocol::None
                    || matches!(&src,super::image::Source::Resource(r) if r.resolver.closed())
                    || super::image::failed(&src, *max_cols, *max_rows, width))
            {
                return render_node(fallback, width, theme, frame);
            }
            super::image::render_with_label(
                &src,
                *max_rows,
                (*max_cols).min(width),
                width,
                theme,
                *image_id,
                path.as_deref(),
            )
        }
        Node::Frames { frames, ms, still } => {
            let pick = if frame == STATIC_FRAME || frames.is_empty() {
                still.as_ref()
            } else {
                &frames[(anim_millis() / (*ms).max(1)) as usize % frames.len()]
            };
            render_node(pick, width, theme, frame)
        }
        Node::Row(items) => {
            let mut l = Line::new(vec![]);
            for n in items {
                if let Some(first) = render_node(n, width, theme, frame).into_iter().next() {
                    l.spans.extend(first.spans);
                }
            }
            vec![truncate_line(&l, width)]
        }
        Node::Spinner(label) => {
            let g = spinner_glyph(theme, frame);
            let mut l = Line::new(vec![Span::styled(format!("{g} "), theme.fg("accent"))]);
            l.spans.extend(label.spans.iter().map(|s| s.resolve(theme)));
            vec![truncate_line(&l, width)]
        }
        Node::Block { title, footer, border_style, border, child } => {
            let set = frame::BorderSet::for_theme(border.as_deref().unwrap_or(&theme.border), &theme.icons);
            let mut o = frame::BoxOpts::new(set, theme.style(border_style));
            if theme.icons == "ascii" {
                o.scrollbar_char = "|";
            }
            o.title = title.iter().map(|s| s.resolve(theme)).collect();
            o.footer = footer.clone();
            o.scrollbar_style = theme.style("MareScrollbar");
            let inner = width.saturating_sub(4).max(1);
            let rows = match child.as_ref() {
                Node::List { items, selected, focused, height } => {
                    let resolved: Vec<Line> = items.iter().map(|l| l.resolve(theme)).collect();
                    let (rows, scroll) = frame::list_rows(&resolved, *selected, *focused, *height, inner, theme);
                    o.scroll = Some(scroll);
                    rows
                }
                c => {
                    let _offset = super::image::row_offset(1);
                    render_node(c, inner, theme, frame)
                }
            };
            frame::boxed(&rows, width, &o)
        }
        Node::List { items, selected, focused, height } => {
            let resolved: Vec<Line> = items.iter().map(|l| l.resolve(theme)).collect();
            frame::list_rows(&resolved, *selected, *focused, *height, width, theme).0
        }
        Node::Split { left, right, ratio, stack_below } => {
            if width < *stack_below {
                let mut out = render_node(left, width, theme, frame);
                let _offset = super::image::row_offset(out.len());
                out.extend(render_node(right, width, theme, frame));
                return out;
            }
            let lw = ((width as f32) * ratio.clamp(0.1, 0.9)) as usize;
            let rw = width.saturating_sub(lw + 1).max(4);
            frame::split(render_node(left, lw, theme, frame), render_node(right, rw, theme, frame), 1)
        }
        Node::Clip { max, tail, child } => {
            // a tool-output preview: the few rows cut from a possibly long output, cached by
            // content (an animated spinner inside can't be cached)
            let clip = || {
                let lines = render_node(child, width, theme, frame);
                if lines.len() <= *max {
                    return lines;
                }
                let hidden = lines.len() - max;
                let hint = Line::styled(
                    format!("… {hidden} more line{} (ctrl+o to expand)", if hidden == 1 { "" } else { "s" }),
                    theme.fg("dim"),
                );
                if *tail {
                    let mut out = vec![hint];
                    out.extend(lines[hidden..].iter().cloned());
                    out
                } else {
                    let mut out: Vec<Line> = lines[..*max].to_vec();
                    out.push(hint);
                    out
                }
            };
            match cache::node_key(node, width) {
                Some(k) => cache::PREVIEW.get_or_insert(k, clip),
                None => clip(),
            }
        }
    }
}

/// Bounded render caches ( cache table's lesson: every cache has a cap, a full
/// cache evicts its least recently used entry — never everything at once). Keys hash the content,
/// the width and the layout generation ([`theme_gen`]: a theme, style, width-rule or `ambiwidth`
/// change), so a hit is exactly what rendering again would produce. A hit is copied out (callers
/// prefix and restyle the lines); the copy is far cheaper than parsing or wrapping again.
pub mod cache {
    use std::hash::{Hash, Hasher};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// The entries and their total line count.
    type Entries = Option<(lru::LruCache<u64, Vec<Line>>, usize)>;

    /// One bounded LRU: at most `cap` entries and `max_lines` rendered lines in all.
    pub struct Lru {
        name: &'static str,
        cap: AtomicUsize,
        max_lines: usize,
        inner: Mutex<Entries>,
    }

    /// Markdown nodes by content hash (tool results, notices, plugin panels; the streaming answer
    /// has its own block cache): 128 renders, 20,000 lines in all. `perf.markdown_cache`.
    pub static MARKDOWN: Lru = Lru::new("markdown", 128, 20_000);
    /// Wrapped paragraphs by (content, width): 1024 paragraphs, 20,000 lines. `perf.wrap_cache`.
    pub static WRAP: Lru = Lru::new("wrap", 1024, 20_000);
    /// Tool-output previews (the clipped compact view of a tool's output): 256 previews, 4,096
    /// lines. `perf.preview_cache`.
    pub static PREVIEW: Lru = Lru::new("preview", 256, 4_096);

    impl Lru {
        const fn new(name: &'static str, cap: usize, max_lines: usize) -> Self {
            Self { name, cap: AtomicUsize::new(cap), max_lines, inner: Mutex::new(None) }
        }

        pub fn name(&self) -> &'static str {
            self.name
        }

        /// Entries cached now (tests, `/profile`).
        pub fn len(&self) -> usize {
            self.inner.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |(c, _)| c.len())
        }

        /// Change the entry cap (`perf.*_cache` in init.lua); 0 turns the cache off.
        pub fn set_capacity(&self, n: usize) {
            self.cap.store(n, Ordering::Relaxed);
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if n == 0 {
                *g = None;
            } else if let Some((c, lines)) = g.as_mut() {
                while c.len() > n {
                    if let Some((_, v)) = c.pop_lru() {
                        *lines -= v.len();
                    }
                }
                c.resize(std::num::NonZeroUsize::new(n).expect("n > 0"));
            }
        }

        pub fn get_or_insert(&self, key: u64, make: impl FnOnce() -> Vec<Line>) -> Vec<Line> {
            let cap = self.cap.load(Ordering::Relaxed);
            if cap == 0 || crate::image::layout_pass_active() {
                return make();
            }
            if let Some(hit) =
                self.inner.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|(c, _)| c.get(&key))
            {
                return hit.clone();
            }
            let waited = crate::image::unsettled();
            let v = make();
            // one render bigger than a quarter of the budget isn't kept (it would evict the rest);
            // nor one with a picture still being prepared (its placeholder would stick)
            if v.len() > self.max_lines / 4 || crate::image::unsettled() != waited {
                return v;
            }
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let (c, lines) =
                g.get_or_insert_with(|| (lru::LruCache::new(std::num::NonZeroUsize::new(cap).expect("cap > 0")), 0));
            // at most `cap` entries (`push` hands back the replaced or evicted entry) and
            // `max_lines` lines
            *lines += v.len();
            if let Some((_, old)) = c.push(key, v.clone()) {
                *lines -= old.len();
            }
            while *lines > self.max_lines {
                match c.pop_lru() {
                    Some((_, old)) => *lines -= old.len(),
                    None => break,
                }
            }
            v
        }
    }

    fn hasher() -> rustc_hash::FxHasher {
        rustc_hash::FxHasher::default()
    }

    /// Key for hashable content at a width under the current layout generation.
    pub fn key<T: Hash + ?Sized>(content: &T, width: usize) -> u64 {
        let mut h = hasher();
        content.hash(&mut h);
        width.hash(&mut h);
        theme_gen().hash(&mut h);
        h.finish()
    }

    /// Key for a node, or `None` when its output changes by itself (a spinner animates, an image
    /// registers its sequence with the write exit when drawn).
    pub fn node_key(node: &Node, width: usize) -> Option<u64> {
        let mut h = hasher();
        hash_node(node, &mut h)?;
        width.hash(&mut h);
        theme_gen().hash(&mut h);
        crate::image::generation().hash(&mut h);
        Some(h.finish())
    }

    fn hash_node(n: &Node, h: &mut rustc_hash::FxHasher) -> Option<()> {
        std::mem::discriminant(n).hash(h);
        match n {
            Node::Empty => {}
            Node::Line(l) => l.hash(h),
            Node::Paragraph { line, indent } => (line, indent).hash(h),
            Node::Lines(items) => {
                items.len().hash(h);
                for i in items {
                    hash_node(i, h)?;
                }
            }
            Node::Markdown(t) => t.hash(h),
            Node::PresentedMarkdown { .. } => return None,
            Node::Unified { diff, path, lines } => (diff, path, lines).hash(h),
            Node::Code { code, lang } => (code, lang).hash(h),
            Node::Diff { old, new, path } => (old, new, path).hash(h),
            Node::Indent { first, rest, child } => {
                (first, rest).hash(h);
                hash_node(child, h)?;
            }
            Node::Gap(k) => k.hash(h),
            Node::Image { .. } | Node::Spinner(_) | Node::Frames { .. } => return None,
            Node::Row(items) => {
                items.len().hash(h);
                for i in items {
                    hash_node(i, h)?;
                }
            }
            Node::Clip { max, tail, child } => {
                (max, tail).hash(h);
                hash_node(child, h)?;
            }
            Node::Block { title, footer, border_style, border, child } => {
                (title, footer, border_style, border).hash(h);
                hash_node(child, h)?;
            }
            Node::List { items, selected, focused, height } => (items, selected, focused, height).hash(h),
            Node::Split { left, right, ratio, stack_below } => {
                (ratio.to_bits(), stack_below).hash(h);
                hash_node(left, h)?;
                hash_node(right, h)?;
            }
        }
        Some(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn bounded_by_entries_and_lines_evicting_the_least_recently_used() {
            let c = Lru::new("t", 3, 10);
            let mk = |n: usize| move || vec![Line::raw("x"); n];
            for k in 0..3 {
                c.get_or_insert(k, mk(2));
            }
            assert_eq!(c.len(), 3);
            c.get_or_insert(0, || panic!("a hit doesn't render again"));
            c.get_or_insert(3, mk(2)); // over the entry cap: 1 (least recently used) goes
            assert_eq!(c.len(), 3);
            let mut rendered = false;
            c.get_or_insert(1, || {
                rendered = true;
                vec![]
            });
            assert!(rendered, "entry 1 was evicted, not 0");
            // over the line budget: old entries go until it fits
            c.get_or_insert(9, mk(2));
            c.get_or_insert(8, mk(2));
            let lines: usize = c.inner.lock().unwrap().as_ref().unwrap().0.iter().map(|(_, v)| v.len()).sum();
            assert!(lines <= 10);
            // too big to keep at all
            c.get_or_insert(7, mk(3));
            assert!(c.inner.lock().unwrap().as_ref().unwrap().0.peek(&7).is_none());
            c.set_capacity(0);
            assert_eq!(c.len(), 0);
        }

        #[test]
        fn a_theme_change_misses() {
            let a = key("same text", 40);
            assert_eq!(a, key("same text", 40));
            assert_ne!(a, key("same text", 41));
            theme_changed();
            assert_ne!(a, key("same text", 40));
            let spin = Node::Spinner(Line::raw("working"));
            assert!(node_key(&spin, 40).is_none());
            assert!(node_key(&Node::Clip { max: 2, tail: false, child: Box::new(spin) }, 40).is_none());
        }
    }
}

/// Split a styled line at '\n' characters into several lines.
pub fn split_line(line: &Line) -> Vec<Line> {
    if !line.spans.iter().any(|s| s.text.contains('\n')) {
        return vec![line.clone()];
    }
    let mut out = vec![Line { spans: vec![], fill_bg: line.fill_bg, raw: None }];
    for s in &line.spans {
        for (i, part) in s.text.split('\n').enumerate() {
            if i > 0 {
                out.push(Line { spans: vec![], fill_bg: line.fill_bg, raw: None });
            }
            out.last_mut().unwrap().push(Span::styled(part, s.style));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gap_rendering_keeps_small_counts_and_caps_oversized_counts() {
        let theme = Theme::default();
        for count in [0, 1, 999, 1_000] {
            assert_eq!(render_node(&Node::Gap(count), 40, &theme, 0).len(), count);
        }
        assert_eq!(render_node(&Node::Gap(1_001), 40, &theme, 0).len(), 1_000);
        assert_eq!(render_node(&Node::Gap(usize::MAX), 40, &theme, 0).len(), 1_000);
    }

    #[test]
    fn large_list_height_materializes_only_existing_items() {
        let theme = Theme::default();
        let list = Node::List { items: vec![Line::raw("one")], selected: Some(0), focused: true, height: usize::MAX };
        assert_eq!(render_node(&list, 40, &theme, 0).len(), 1);
        let block = Node::Block {
            title: Vec::new(),
            footer: None,
            border_style: "MareBorder".into(),
            border: None,
            child: Box::new(list),
        };
        assert_eq!(render_node(&block, 40, &theme, 0).len(), 3);
    }

    #[test]
    fn a_clipped_preview_counts_its_hidden_lines_in_the_right_number() {
        let theme = Theme::default();
        let text = |lines: &[Line]| {
            lines.iter().map(|l| l.spans.iter().map(|s| s.text.as_str()).collect::<String>()).collect::<Vec<_>>()
        };
        let rows =
            |n: usize| Box::new(Node::Lines((0..n).map(|i| Node::Line(Line::raw(format!("row {i}")))).collect()));
        let one = text(&render_node(&Node::Clip { max: 2, tail: false, child: rows(3) }, 40, &theme, 0));
        assert_eq!(one.last().unwrap(), "… 1 more line (ctrl+o to expand)");
        let two = text(&render_node(&Node::Clip { max: 2, tail: true, child: rows(4) }, 40, &theme, 0));
        assert_eq!(two[0], "… 2 more lines (ctrl+o to expand)");
    }

    #[test]
    fn frames_play_by_the_clock_hold_still_and_sit_in_a_row() {
        let theme = Theme::default();
        let line = |s: &str| Node::Line(Line::raw(s));
        let anim = Node::Frames { frames: vec![line("a"), line("b")], ms: 1_000_000, still: Box::new(line("=")) };
        let row = Node::Row(vec![anim, line(" label"), Node::Lines(vec![line(" tail"), line("dropped")])]);
        let text = |frame| {
            render_node(&row, 40, &theme, frame)
                .iter()
                .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect::<String>())
                .collect::<Vec<_>>()
        };
        // one row; the first line of each child; frame by wall clock (a 1000 s frame: still "a")
        assert_eq!(text(0), ["a label tail"]);
        assert_eq!(text(STATIC_FRAME), ["= label tail"], "reduce_motion: the still frame");
        assert!(cache::node_key(&row, 40).is_none(), "an animation is never cached");
    }

    #[test]
    fn spinner_wall_clock_advances_with_clock_and_keeps_explicit_and_static_deterministic() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static OFFSET_MS: AtomicU64 = AtomicU64::new(0);
        static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        fn mock_now() -> std::time::Instant {
            *BASE.get_or_init(std::time::Instant::now)
                + std::time::Duration::from_millis(OFFSET_MS.load(Ordering::Relaxed))
        }

        let theme = Theme::default();
        OFFSET_MS.store(0, Ordering::Relaxed);
        set_clock(mock_now);

        let g0 = spinner_glyph(&theme, WALL_CLOCK_FRAME).to_string();
        assert_eq!(g0, spinner_glyph(&theme, 0));

        // Advance mock clock by 80ms -> WALL_CLOCK_FRAME advances without changing `frame` arg!
        OFFSET_MS.store(80, Ordering::Relaxed);
        let g1 = spinner_glyph(&theme, WALL_CLOCK_FRAME).to_string();
        assert_ne!(g1, g0, "WALL_CLOCK_FRAME must advance after 80ms on animation clock");
        assert_eq!(g1, spinner_glyph(&theme, 1));

        // Explicit snapshot index 0 and STATIC_FRAME stay deterministic regardless of clock.
        assert_eq!(spinner_glyph(&theme, 0), g0);
        assert_eq!(spinner_glyph(&theme, STATIC_FRAME), theme.spinner_glyph(0, true));

        // Restore real clock for subsequent tests on this thread.
        set_clock(std::time::Instant::now);
    }
}
