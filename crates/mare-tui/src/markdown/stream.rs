//! Streaming: the stable split of a growing message and [`StreamingMarkdown`], which caches the
//! completed blocks and re-renders only the tail.

use super::*;

/// Returns the byte index at which `src` can be split so that the prefix consists only of
/// completed blocks (split happens right after a blank line outside any code fence, and the next
/// block does not look like a continuation of a list / quote / indented content).
pub fn stable_split(src: &str) -> usize {
    let mut fence: Option<(char, usize)> = None;
    let mut split = 0;
    let mut offset = 0;
    let mut prev_blank = false;
    let mut blank_end = 0;
    for line in src.split_inclusive('\n') {
        let complete = line.ends_with('\n');
        let content = line.trim_end_matches(['\n', '\r']);
        let trimmed = content.trim_start();
        let indent = content.len() - trimmed.len();
        if prev_blank && fence.is_none() && complete && !trimmed.is_empty() && indent == 0 && !is_continuation(trimmed)
        {
            split = blank_end;
        }
        if indent < 4 {
            let fc = trimmed.chars().next();
            if let Some(c @ ('`' | '~')) = fc {
                let n = trimmed.chars().take_while(|&x| x == c).count();
                if n >= 3 {
                    match fence {
                        None if c == '~' || !trimmed[n..].contains('`') => fence = Some((c, n)),
                        Some((fc, fnn)) if fc == c && n >= fnn && trimmed[n..].trim().is_empty() => fence = None,
                        _ => {}
                    }
                }
            }
        }
        offset += line.len();
        prev_blank = complete && trimmed.is_empty() && fence.is_none();
        if prev_blank {
            blank_end = offset;
        }
    }
    split
}

fn is_continuation(t: &str) -> bool {
    if t.starts_with('>') || t.starts_with('|') {
        return true;
    }
    let b = t.as_bytes();
    if matches!(b[0], b'-' | b'*' | b'+') && (b.len() == 1 || b[1] == b' ') {
        return true;
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    digits > 0 && matches!(b.get(digits), Some(b'.') | Some(b')'))
}

/// Incremental state for a top-level append-only open code fence in the unstable tail.
struct OpenFence {
    header_end: usize,
    scan_from: usize,
    fence_char: char,
    fence_len: usize,
    lang: String,
    card_style: super::code::CodeCardStyle,
    nw: usize,
    completed_cards: Vec<Line>,
    highlight: super::super::highlight::StreamingHighlight,
}

fn contains_closing_fence(slice: &str, fc: char, flen: usize) -> bool {
    for line in slice.split('\n') {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        if indent <= 3 {
            let n = trimmed.bytes().take_while(|&b| b == fc as u8).count();
            if n >= flen && trimmed[n..].trim_matches([' ', '\t']).is_empty() {
                return true;
            }
        }
    }
    false
}

fn try_open_fence(tail: &str) -> Option<(char, usize, String, usize)> {
    if tail.contains('\r') || tail.contains('\0') {
        return None;
    }
    let nl = tail.find('\n')?;
    let first_line = &tail[..nl];
    let first_byte = *first_line.as_bytes().first()?;
    if first_byte != b'`' && first_byte != b'~' {
        return None;
    }
    let fc = first_byte as char;
    let flen = first_line.bytes().take_while(|&b| b == first_byte).count();
    if flen < 3 {
        return None;
    }
    let info = &first_line[flen..];
    if fc == '`' && info.contains('`') {
        return None;
    }
    if info.contains('\\') || info.contains('&') {
        return None;
    }
    let header_end = nl + 1;
    if contains_closing_fence(&tail[header_end..], fc, flen) {
        return None;
    }
    let lang = info.split_whitespace().next().unwrap_or("").to_string();
    Some((fc, flen, lang, header_end))
}

/// Incremental renderer for a growing markdown message. The rendered lines of the stable prefix
/// (completed blocks) are cached per width; only the tail block is re-rendered on every update.
#[derive(Default)]
pub struct StreamingMarkdown {
    text: String,
    cache_src_len: usize,
    cache_width: usize,
    cache: Vec<Line>,
    /// footnote definitions of the stable prefix: shown after the tail, not part of `cache`
    cache_notes: Vec<Line>,
    /// [`super::image::generation`] the cache was built under (pictures at that size)
    cache_images: u64,
    cache_theme: Option<Theme>,
    cache_theme_gen: u64,
    /// The tail and notes for an unchanged frame; unstable images must never enter this cache.
    frame: Option<Vec<Line>>,
    open_fence: Option<OpenFence>,
    highlight: super::super::highlight::StreamingHighlight,
}

impl StreamingMarkdown {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_text(&mut self, src: &str) {
        if self.text == src {
            return;
        }
        self.frame = None;
        if !src.starts_with(&self.text) {
            self.highlight = Default::default();
            self.open_fence = None;
        }
        if !src.starts_with(&self.text[..self.cache_src_len.min(self.text.len())]) {
            self.cache_src_len = 0;
            self.cache.clear();
            self.cache_notes.clear();
        }
        self.text = src.to_string();
    }

    pub fn push(&mut self, delta: &str) {
        if !delta.is_empty() {
            self.frame = None;
        }
        self.text.push_str(delta);
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Release rendered lines (committed blocks keep only their source; Ctrl+T re-renders).
    pub fn drop_cache(&mut self) {
        self.cache = Vec::new();
        self.cache_notes = Vec::new();
        self.cache_src_len = 0;
        self.cache_width = 0;
        self.cache_theme = None;
        self.frame = None;
        self.open_fence = None;
        self.highlight = Default::default();
    }

    /// Lines (from the last `render`) that belong to the stable prefix and won't change.
    pub fn stable_lines(&self) -> usize {
        self.cache.len()
    }

    /// Rows the first `src` bytes render to at `width` (resize: committed rows from the
    /// committed source position). Footnotes are not counted: they stay below the tail.
    pub fn rows_for(&self, src: usize, width: usize, theme: &Theme) -> usize {
        let src = src.min(self.text.len());
        if src == 0 { 0 } else { render_parts(&self.text[..src], width, theme).0.len() }
    }

    /// Source bytes covered by [`StreamingMarkdown::stable_lines`] (completed top-level blocks).
    pub fn stable_src(&self) -> usize {
        self.cache_src_len
    }

    pub fn render(&mut self, width: usize, theme: &Theme) -> Vec<Line> {
        self.render_from(width, theme, 0)
    }

    fn render_open_fence(
        open: &mut OpenFence,
        text: &str,
        caches: (&[Line], &[Line]),
        width: usize,
        theme: &Theme,
        skip: usize,
    ) -> Vec<Line> {
        let (cache, cache_notes) = caches;
        open.scan_from = text.rfind('\n').map_or(open.header_end, |p| (p + 1).max(open.header_end));
        let body_raw = &text[open.header_end..];
        let code_text = body_raw.strip_suffix('\n').unwrap_or(body_raw);
        let avail = width.max(4);
        let (completed, tail_line) = open.highlight.update(code_text, &open.lang, theme, true);
        let total_lines = (completed.len() + usize::from(tail_line.is_some())).max(1);
        let nw = total_lines.to_string().len();
        if nw != open.nw || open.completed_cards.len() > completed.len() {
            open.completed_cards.clear();
            open.completed_cards.reserve(completed.len());
            for (i, l) in completed.iter().enumerate() {
                open.completed_cards.push(super::code::format_code_card_line(l.spans.clone(), i, nw, &open.card_style));
            }
            open.nw = nw;
        } else if open.completed_cards.len() < completed.len() {
            for (i, l) in completed.iter().enumerate().skip(open.completed_cards.len()) {
                open.completed_cards.push(super::code::format_code_card_line(l.spans.clone(), i, nw, &open.card_style));
            }
        }
        let tail_card = if let Some(tl) = tail_line {
            Some(super::code::format_code_card_line(tl.spans, completed.len(), nw, &open.card_style))
        } else if open.completed_cards.is_empty() {
            Some(super::code::format_code_card_line(Vec::new(), 0, nw, &open.card_style))
        } else {
            None
        };
        let label = super::code::code_label(&open.lang, true, &open.card_style);
        let first_w = open.completed_cards.first().or(tail_card.as_ref()).map(Line::width).unwrap_or(0);
        let extra_label = super::code::top_label_row(first_w, avail, &label, &open.card_style);
        let has_extra_label = extra_label.is_some();
        let has_lead_blank = !cache.is_empty();
        let has_notes = !cache_notes.is_empty();
        let body_len = open.completed_cards.len() + usize::from(tail_card.is_some());
        let total = cache.len()
            + usize::from(has_lead_blank)
            + 1
            + usize::from(has_extra_label)
            + body_len
            + 1
            + if has_notes { 1 + cache_notes.len() } else { 0 };
        if skip >= total {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(total - skip);
        let mut pos = 0usize;
        if skip < cache.len() {
            out.extend(cache[skip..].iter().cloned());
        }
        pos += cache.len();
        fn push_one(out: &mut Vec<Line>, pos: &mut usize, skip: usize, line: Line) {
            if *pos >= skip {
                out.push(line);
            }
            *pos += 1;
        }
        if has_lead_blank {
            push_one(&mut out, &mut pos, skip, Line::empty());
        }
        push_one(
            &mut out,
            &mut pos,
            skip,
            super::code::code_edge_line(&open.card_style.top_glyph, avail, open.card_style.edge),
        );
        if let Some(extra) = extra_label {
            push_one(&mut out, &mut pos, skip, extra);
        }
        if !open.completed_cards.is_empty() {
            if !has_extra_label && !label.is_empty() {
                // Label goes inline on row 0 when it fits.
                if pos >= skip {
                    let mut row0 = open.completed_cards[0].clone();
                    super::code::attach_inline_label(&mut row0, first_w, avail, &label, &open.card_style);
                    out.push(row0);
                }
                pos += 1;
                let rest_start = skip.saturating_sub(pos);
                if rest_start < open.completed_cards.len() - 1 {
                    out.extend(open.completed_cards[1 + rest_start..].iter().cloned());
                }
                pos += open.completed_cards.len() - 1;
            } else {
                let start = skip.saturating_sub(pos);
                if start < open.completed_cards.len() {
                    out.extend(open.completed_cards[start..].iter().cloned());
                }
                pos += open.completed_cards.len();
            }
            if let Some(tc) = tail_card {
                push_one(&mut out, &mut pos, skip, tc);
            }
        } else if let Some(mut tc) = tail_card {
            if !has_extra_label {
                super::code::attach_inline_label(&mut tc, first_w, avail, &label, &open.card_style);
            }
            push_one(&mut out, &mut pos, skip, tc);
        }
        push_one(
            &mut out,
            &mut pos,
            skip,
            super::code::code_edge_line(&open.card_style.bottom_glyph, avail, open.card_style.edge),
        );
        if has_notes {
            push_one(&mut out, &mut pos, skip, Line::empty());
            let start = skip.saturating_sub(pos);
            if start < cache_notes.len() {
                out.extend(cache_notes[start..].iter().cloned());
            }
        }
        out
    }

    /// The rendered rows from row `skip` on — what a streaming block still shows live once its
    /// first `skip` rows are in the scrollback. Only the source after the stable prefix is
    /// scanned and parsed, and only the rows asked for are copied.
    pub fn render_from(&mut self, width: usize, theme: &Theme, skip: usize) -> Vec<Line> {
        if super::image::layout_pass_active() && mare_shared::markdown::has_images(&self.text) {
            return render_markdown(&self.text, width, theme).into_iter().skip(skip).collect();
        }
        // a stable split is right after a blank line outside any fence, where the scanner's
        // state is fresh: scanning on from the cached split finds the same later splits
        let images = super::image::generation();
        let theme_gen = mare_shared::style::theme_gen();
        let fresh = width == self.cache_width
            && images == self.cache_images
            && theme_gen == self.cache_theme_gen
            && self.cache_theme.as_ref() == Some(theme);
        if fresh && let Some(rest) = &self.frame {
            return self.cache.iter().chain(rest).skip(skip).cloned().collect();
        }
        if fresh
            && let Some(open) = self.open_fence.as_mut()
            && self.text.len() >= open.scan_from
        {
            let delta = &self.text[open.scan_from..];
            if !delta.contains('\r')
                && !delta.contains('\0')
                && !contains_closing_fence(delta, open.fence_char, open.fence_len)
            {
                return Self::render_open_fence(open, &self.text, (&self.cache, &self.cache_notes), width, theme, skip);
            }
            self.open_fence = None;
            self.highlight = Default::default();
        } else if !fresh {
            self.open_fence = None;
            self.highlight = Default::default();
        }
        let from = if fresh { self.cache_src_len.min(self.text.len()) } else { 0 };
        let split = from + stable_split(&self.text[from..]);
        let waited = super::image::unsettled();
        if !fresh || split < self.cache_src_len {
            (self.cache, self.cache_notes) =
                if split == 0 { Default::default() } else { render_parts(&self.text[..split], width, theme) };
            self.cache_src_len = split;
            self.cache_width = width;
            self.cache_images = images;
            self.cache_theme = Some(theme.clone());
            self.cache_theme_gen = theme_gen;
        } else if split > self.cache_src_len {
            // only the newly completed blocks are parsed; earlier blocks stay cached
            let (more, notes) = render_parts(&self.text[self.cache_src_len..split], width, theme);
            join_blocks(&mut self.cache, more);
            join_blocks(&mut self.cache_notes, notes);
            self.cache_src_len = split;
            self.highlight = Default::default();
        }
        // a picture of the stable prefix is still being prepared: its placeholder is in the
        // cache — build it all again next time (it's only kept for this frame)
        if super::image::unsettled() != waited {
            self.cache_width = 0;
        }
        if self.cache_width == width
            && let Some((fc, flen, lang, rel_header_end)) = try_open_fence(&self.text[split..])
        {
            self.highlight = Default::default();
            let mut open = OpenFence {
                header_end: split + rel_header_end,
                scan_from: split + rel_header_end,
                fence_char: fc,
                fence_len: flen,
                lang,
                card_style: super::code::code_card_style(theme),
                nw: 0,
                completed_cards: Vec::new(),
                highlight: Default::default(),
            };
            let out =
                Self::render_open_fence(&mut open, &self.text, (&self.cache, &self.cache_notes), width, theme, skip);
            self.open_fence = Some(open);
            return out;
        }
        // footnotes stay after the growing tail: at the end of the answer
        let (tail, tail_notes) = render_parts_streaming(&self.text[split..], width, theme, Some(&mut self.highlight));
        let mut rest = tail;
        let mut notes = self.cache_notes.clone();
        join_blocks(&mut notes, tail_notes);
        join_blocks(&mut rest, notes);
        if !self.cache.is_empty() && !rest.is_empty() {
            rest.insert(0, Line::empty());
        }
        if super::image::unsettled() == waited {
            let frame = self.frame.insert(rest);
            self.cache.iter().chain(frame.iter()).skip(skip).cloned().collect()
        } else {
            self.frame = None;
            self.cache.iter().chain(rest.iter()).skip(skip).cloned().collect()
        }
    }
}
