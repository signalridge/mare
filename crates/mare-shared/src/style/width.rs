//! Display width in terminal cells: grapheme clusters, width overrides, `ambiwidth` and the
//! per-terminal width method.

use super::*;

/// Display width of a string in terminal cells.
pub fn str_width(s: &str) -> usize {
    // Tabs are expanded by callers; count them as 4 here for safety.
    s.graphemes(true).map(grapheme_width).sum()
}

/// Width overrides `(first code point, last code point, width)` from the `char_widths` option
/// . Checked by the first char of a
/// grapheme; empty by default (no cost).
static WIDTH_OVERRIDES: std::sync::RwLock<Vec<(u32, u32, u8)>> = std::sync::RwLock::new(Vec::new());
static HAS_WIDTH_OVERRIDES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_width_overrides(mut ranges: Vec<(u32, u32, u8)>) {
    ranges.retain(|(a, b, w)| a <= b && *w <= 2);
    HAS_WIDTH_OVERRIDES.store(!ranges.is_empty(), std::sync::atomic::Ordering::Release);
    *WIDTH_OVERRIDES.write().unwrap_or_else(|e| e.into_inner()) = ranges;
    theme_changed();
}

/// `mare.o.terminal.ambiwidth`: East Asian *ambiguous*-width characters
/// (`•`, `◦`, `▪`, `✓`, `ⓘ`, `⚠`, box drawing in some fonts, …) take two cells when "double".
static AMBI_DOUBLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_ambiwidth_double(double: bool) {
    if AMBI_DOUBLE.swap(double, std::sync::atomic::Ordering::AcqRel) != double {
        theme_changed();
    }
}

pub fn ambiwidth_double() -> bool {
    AMBI_DOUBLE.load(std::sync::atomic::Ordering::Acquire)
}

/// How the terminal measures a grapheme cluster ( 宽度和换行; OpenTUI
/// `native/src/terminal.zig` `WidthMethod`): chosen per terminal at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidthMethod {
    /// one width per grapheme cluster (an emoji ZWJ sequence or a VS16 emoji is 2 cells):
    /// terminals with mode 2027 and the modern ones that draw clusters the same way
    Unicode,
    /// the sum of every code point's `wcwidth` (a ZWJ family is 6 cells, `☀️` is 1): tmux,
    /// screen, Terminal.app and other terminals that measure code point by code point
    Wcwidth,
}

impl WidthMethod {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "unicode" | "grapheme" => Some(Self::Unicode),
            "wcwidth" => Some(Self::Wcwidth),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Unicode => "unicode",
            Self::Wcwidth => "wcwidth",
        }
    }
}

static WCWIDTH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set the width method (the startup probe, `terminal_caps`, `MARE_WIDTH_METHOD`).
pub fn set_width_method(m: WidthMethod) {
    if WCWIDTH.swap(m == WidthMethod::Wcwidth, std::sync::atomic::Ordering::AcqRel) != (m == WidthMethod::Wcwidth) {
        theme_changed();
    }
}

pub fn width_method() -> WidthMethod {
    if WCWIDTH.load(std::sync::atomic::Ordering::Acquire) { WidthMethod::Wcwidth } else { WidthMethod::Unicode }
}

fn is_pua(c: char) -> bool {
    matches!(c, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}' | '\u{100000}'..='\u{10FFFD}')
}

/// `wcwidth` of every code point of a cluster, summed (zero-width ones count 0).
pub(super) fn wcwidth_sum(g: &str) -> usize {
    let cjk = ambiwidth_double();
    g.chars()
        .map(|c| {
            if cjk && !is_pua(c) {
                unicode_width::UnicodeWidthChar::width_cjk(c).unwrap_or(0)
            } else {
                unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
            }
        })
        .sum()
}

pub fn grapheme_width(g: &str) -> usize {
    if g == "\t" {
        return 4;
    }
    if HAS_WIDTH_OVERRIDES.load(std::sync::atomic::Ordering::Acquire)
        && let Some(c) = g.chars().next()
        && let Some((_, _, w)) = WIDTH_OVERRIDES
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .find(|(a, b, _)| (*a..=*b).contains(&(c as u32)))
    {
        return *w as usize;
    }
    if WCWIDTH.load(std::sync::atomic::Ordering::Relaxed) {
        return wcwidth_sum(g);
    }
    cluster_width(g)
}

/// Width of one grapheme cluster (MIT): RGI emoji and regional indicators
/// take 2 cells; combining marks are zero width; Indic spacing marks, a consonant after a
/// virama, half/full-width forms and the Thai/Lao AM vowels add cells the way terminals draw them.
fn cluster_width(g: &str) -> usize {
    use unicode_properties::{GeneralCategoryGroup, UnicodeEmoji, UnicodeGeneralCategory};
    let b = g.as_bytes();
    if b.len() == 1 && (0x20..=0x7e).contains(&b[0]) {
        return 1;
    }
    let is_mark = |c: char| c.general_category_group() == GeneralCategoryGroup::Mark;
    let spacing_mark = |c: char| {
        (c.general_category() == unicode_properties::GeneralCategory::SpacingMark
            && !matches!(c, '\u{1734}' | '\u{302E}' | '\u{302F}'))
            || matches!(c, '\u{065F}' | '\u{0F7F}' | '\u{102B}' | '\u{102C}' | '\u{1031}' | '\u{1033}'..='\u{1035}' | '\u{1038}' | '\u{103A}'..='\u{103E}')
    };
    let non_printing = |c: char| {
        (matches!(c.general_category_group(), GeneralCategoryGroup::Mark | GeneralCategoryGroup::Other)
            && c.general_category() != unicode_properties::GeneralCategory::PrivateUse)
            || matches!(c, '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}' | '\u{200B}'..='\u{200F}')
    };
    // marks some terminals give a cell even without a base character
    if g.chars().all(spacing_mark) {
        return g.chars().count();
    }
    if g.chars().all(non_printing) {
        return 0;
    }
    // emoji: a pictograph with emoji presentation, or turned into one by VS16 / a ZWJ sequence
    let mut it = g.chars();
    let first = it.next().unwrap_or(' ');
    let emoji_seq =
        g.contains('\u{FE0F}') || g.contains('\u{200D}') || g.chars().any(|c| matches!(c, '\u{1F3FB}'..='\u{1F3FF}'));
    if (first.is_emoji_char() && (emoji_seq || UnicodeWidthStr::width(first.encode_utf8(&mut [0; 4]) as &str) == 2))
        || ('\u{1F1E6}'..='\u{1F1FF}').contains(&first)
    {
        return 2;
    }
    let base_idx = g.char_indices().find(|(_, c)| !non_printing(*c)).map(|(i, _)| i).unwrap_or(0);
    let base = &g[base_idx..];
    let mut chars = base.chars();
    let Some(bc) = chars.next() else { return 0 };
    let mut w = if ambiwidth_double() && !is_pua(bc) {
        unicode_width::UnicodeWidthChar::width_cjk(bc).unwrap_or(0)
    } else {
        unicode_width::UnicodeWidthChar::width(bc).unwrap_or(0)
    };
    let mut follows_mark = false;
    for c in chars {
        if spacing_mark(c) {
            w += 1;
            follows_mark = false;
        } else if is_mark(c) {
            follows_mark = true;
        } else if !non_printing(c) {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if follows_mark || ('\u{FF00}'..='\u{FFEF}').contains(&c) {
                w += cw;
            } else if matches!(c, '\u{0E33}' | '\u{0EB3}') {
                w += 1;
            }
            follows_mark = false;
        }
    }
    w
}
