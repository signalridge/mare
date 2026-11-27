//! The styled-text model: `Color`, `Style` (with interned hyperlinks), `Span` and `Line`.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub enum Color {
    #[default]
    Reset,
    Rgb(u8, u8, u8),
    Ansi(u8),
    /// Deferred reference to a named theme style/color (interned via [`theme_name_id`]),
    /// resolved at render time against the active [`Theme`].
    Theme(u16),
}

static THEME_NAMES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Intern a named theme style/color for deferred render-time resolution. Ids are 1-based (`0` = none).
pub fn theme_name_id(name: &str) -> u16 {
    let mut v = THEME_NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = v.iter().position(|n| n == name) {
        return (i + 1) as u16;
    }
    if v.len() >= u16::MAX as usize {
        return 0;
    }
    v.push(name.to_string());
    v.len() as u16
}

/// Look up an interned theme style/color name previously registered via [`theme_name_id`].
pub fn theme_name(id: u16) -> Option<String> {
    if id == 0 {
        return None;
    }
    THEME_NAMES.lock().unwrap_or_else(|e| e.into_inner()).get(id as usize - 1).cloned()
}

impl Color {
    /// Resolve a symbolic [`Color::Theme`] against `theme`; literal colors are returned unchanged.
    pub fn resolve(self, theme: &Theme) -> Self {
        match self {
            Self::Theme(id) => theme_name(id).map(|n| theme.get(&n)).unwrap_or(Self::Reset),
            other => other,
        }
    }

    /// Parse `#rrggbb`, `#rgb`, `ansi:N`/plain number, or a basic color name.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix('#') {
            let v = match hex.len() {
                6 => u32::from_str_radix(hex, 16).ok()?,
                3 => {
                    let v = u32::from_str_radix(hex, 16).ok()?;
                    let (r, g, b) = ((v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
                    (r * 17) << 16 | (g * 17) << 8 | (b * 17)
                }
                _ => return None,
            };
            return Some(Self::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8));
        }
        if let Ok(n) = s.trim_start_matches("ansi:").parse::<u8>() {
            return Some(Self::Ansi(n));
        }
        let n = match s {
            "reset" | "default" => return Some(Self::Reset),
            "black" => 0,
            "red" => 1,
            "green" => 2,
            "yellow" => 3,
            "blue" => 4,
            "magenta" => 5,
            "cyan" => 6,
            "white" => 7,
            "gray" | "grey" | "bright_black" => 8,
            "bright_red" => 9,
            "bright_green" => 10,
            "bright_yellow" => 11,
            "bright_blue" => 12,
            "bright_magenta" => 13,
            "bright_cyan" => 14,
            "bright_white" => 15,
            _ => return None,
        };
        Some(Self::Ansi(n))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub reverse: bool,
    /// OSC 8 hyperlink target (interned id from [`link_id`]; 0 = none)
    pub link: u32,
    /// Deferred named highlight group id (`0` = none), resolved on render against live [`Theme`].
    pub hl: u16,
    /// Optional base highlight group layered underneath `hl` (e.g. `Line::base_style`).
    pub base_hl: u16,
    /// Bitmask of boolean attributes explicitly forced to `false` on top of `hl`/`base_hl`.
    pub clear_attrs: u8,
}

const ATTR_BOLD: u8 = 1 << 0;
const ATTR_DIM: u8 = 1 << 1;
const ATTR_ITALIC: u8 = 1 << 2;
const ATTR_UNDERLINE: u8 = 1 << 3;
const ATTR_STRIKE: u8 = 1 << 4;
const ATTR_REVERSE: u8 = 1 << 5;

static LINKS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Intern a hyperlink URL for [`Style::link`]. Ids are stable for the process lifetime.
pub fn link_id(url: &str) -> u32 {
    let mut v = LINKS.lock().unwrap();
    if let Some(i) = v.iter().rposition(|u| u == url) {
        return i as u32 + 1;
    }
    v.push(url.to_string());
    v.len() as u32
}

pub(super) fn link_url(id: u32) -> Option<String> {
    if id == 0 {
        return None;
    }
    LINKS.lock().unwrap().get(id as usize - 1).cloned()
}

impl Style {
    pub const fn new() -> Self {
        Self {
            fg: None,
            bg: None,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            strike: false,
            reverse: false,
            link: 0,
            hl: 0,
            base_hl: 0,
            clear_attrs: 0,
        }
    }
    /// Symbolic named highlight group (or literal color when `name` parses as a [`Color`]).
    /// Deferred until render so theme changes recolor existing nodes cleanly.
    pub fn named(name: &str) -> Self {
        if let Some(c) = Color::parse(name) {
            return Self::new().fg(c);
        }
        Self { hl: theme_name_id(name), ..Self::new() }
    }
    /// Explicitly set or clear a boolean attribute (recording `false` in `clear_attrs` so it
    /// overrides attributes inherited from a deferred highlight group).
    pub fn with_bold(mut self, on: bool) -> Self {
        self.bold = on;
        if on {
            self.clear_attrs &= !ATTR_BOLD
        } else {
            self.clear_attrs |= ATTR_BOLD
        }
        self
    }
    pub fn with_dim(mut self, on: bool) -> Self {
        self.dim = on;
        if on {
            self.clear_attrs &= !ATTR_DIM
        } else {
            self.clear_attrs |= ATTR_DIM
        }
        self
    }
    pub fn with_italic(mut self, on: bool) -> Self {
        self.italic = on;
        if on {
            self.clear_attrs &= !ATTR_ITALIC
        } else {
            self.clear_attrs |= ATTR_ITALIC
        }
        self
    }
    pub fn with_underline(mut self, on: bool) -> Self {
        self.underline = on;
        if on {
            self.clear_attrs &= !ATTR_UNDERLINE
        } else {
            self.clear_attrs |= ATTR_UNDERLINE
        }
        self
    }
    pub fn with_strike(mut self, on: bool) -> Self {
        self.strike = on;
        if on {
            self.clear_attrs &= !ATTR_STRIKE
        } else {
            self.clear_attrs |= ATTR_STRIKE
        }
        self
    }
    pub fn with_reverse(mut self, on: bool) -> Self {
        self.reverse = on;
        if on {
            self.clear_attrs &= !ATTR_REVERSE
        } else {
            self.clear_attrs |= ATTR_REVERSE
        }
        self
    }
    /// Hyperlink target (OSC 8). Only validated http/https URLs become links; anything else
    /// stays plain text.
    pub fn link(mut self, url: &str) -> Self {
        self.link = if crate::ansi::valid_link(url) { link_id(url) } else { 0 };
        self
    }
    pub fn fg(mut self, c: Color) -> Self {
        self.fg = Some(c);
        self
    }
    pub fn bg(mut self, c: Color) -> Self {
        self.bg = Some(c);
        self
    }
    pub fn bold(mut self) -> Self {
        self.bold = true;
        self.clear_attrs &= !ATTR_BOLD;
        self
    }
    pub fn dim(mut self) -> Self {
        self.dim = true;
        self.clear_attrs &= !ATTR_DIM;
        self
    }
    pub fn italic(mut self) -> Self {
        self.italic = true;
        self.clear_attrs &= !ATTR_ITALIC;
        self
    }
    pub fn underline(mut self) -> Self {
        self.underline = true;
        self.clear_attrs &= !ATTR_UNDERLINE;
        self
    }
    pub fn strike(mut self) -> Self {
        self.strike = true;
        self.clear_attrs &= !ATTR_STRIKE;
        self
    }
    pub fn reverse(mut self) -> Self {
        self.reverse = true;
        self.clear_attrs &= !ATTR_REVERSE;
        self
    }
    /// Overlay `other` on top of `self` (other's set fields win).
    pub fn patch(mut self, other: Self) -> Self {
        if other.hl != 0 {
            if other.base_hl != 0 {
                self.base_hl = other.base_hl;
            } else if self.hl != 0 {
                self.base_hl = self.hl;
            }
            self.hl = other.hl;
        }
        if other.fg.is_some() {
            self.fg = other.fg;
        }
        if other.bg.is_some() {
            self.bg = other.bg;
        }
        if other.clear_attrs != 0 {
            if other.clear_attrs & ATTR_BOLD != 0 {
                self.bold = false;
            }
            if other.clear_attrs & ATTR_DIM != 0 {
                self.dim = false;
            }
            if other.clear_attrs & ATTR_ITALIC != 0 {
                self.italic = false;
            }
            if other.clear_attrs & ATTR_UNDERLINE != 0 {
                self.underline = false;
            }
            if other.clear_attrs & ATTR_STRIKE != 0 {
                self.strike = false;
            }
            if other.clear_attrs & ATTR_REVERSE != 0 {
                self.reverse = false;
            }
            self.clear_attrs |= other.clear_attrs;
        }
        if other.bold {
            self.bold = true;
            self.clear_attrs &= !ATTR_BOLD;
        }
        if other.dim {
            self.dim = true;
            self.clear_attrs &= !ATTR_DIM;
        }
        if other.italic {
            self.italic = true;
            self.clear_attrs &= !ATTR_ITALIC;
        }
        if other.underline {
            self.underline = true;
            self.clear_attrs &= !ATTR_UNDERLINE;
        }
        if other.strike {
            self.strike = true;
            self.clear_attrs &= !ATTR_STRIKE;
        }
        if other.reverse {
            self.reverse = true;
            self.clear_attrs &= !ATTR_REVERSE;
        }
        if other.link != 0 {
            self.link = other.link;
        }
        self
    }
    /// Materialize deferred theme styles/colors and gate OSC 8 links on `theme.md.osc8`.
    pub fn resolve(self, theme: &Theme) -> Self {
        let needs_resolve = self.hl != 0
            || self.base_hl != 0
            || self.clear_attrs != 0
            || matches!(self.fg, Some(Color::Theme(_)))
            || matches!(self.bg, Some(Color::Theme(_)))
            || (self.link != 0 && !theme.md.osc8);
        if !needs_resolve {
            return self;
        }
        let mut out = Self::new();
        if self.base_hl != 0
            && let Some(name) = theme_name(self.base_hl)
        {
            out = theme.style(&name);
        }
        if self.hl != 0
            && let Some(name) = theme_name(self.hl)
        {
            out = out.patch(theme.style(&name));
        }
        if self.clear_attrs & ATTR_BOLD != 0 {
            out.bold = false;
        }
        if self.clear_attrs & ATTR_DIM != 0 {
            out.dim = false;
        }
        if self.clear_attrs & ATTR_ITALIC != 0 {
            out.italic = false;
        }
        if self.clear_attrs & ATTR_UNDERLINE != 0 {
            out.underline = false;
        }
        if self.clear_attrs & ATTR_STRIKE != 0 {
            out.strike = false;
        }
        if self.clear_attrs & ATTR_REVERSE != 0 {
            out.reverse = false;
        }
        out.bold |= self.bold;
        out.dim |= self.dim;
        out.italic |= self.italic;
        out.underline |= self.underline;
        out.strike |= self.strike;
        out.reverse |= self.reverse;
        if let Some(c) = self.fg {
            out.fg = Some(c.resolve(theme));
        }
        if let Some(c) = self.bg {
            out.bg = Some(c.resolve(theme));
        }
        out.link = if theme.md.osc8 { self.link } else { 0 };
        out.hl = 0;
        out.base_hl = 0;
        out.clear_attrs = 0;
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Hash)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn raw(text: impl Into<String>) -> Self {
        Self { text: text.into(), style: Style::default() }
    }
    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self { text: text.into(), style }
    }
    pub fn width(&self) -> usize {
        str_width(&self.text)
    }
    pub fn resolve(&self, theme: &Theme) -> Self {
        Self { text: self.text.clone(), style: self.style.resolve(theme) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Hash)]
pub struct Line {
    pub spans: Vec<Span>,
    /// When set, the remainder of the row (after the spans) is painted with this background.
    pub fill_bg: Option<Color>,
    /// Bytes written after the row's text — only core-built image sequences,
    /// registered with the write exit so its final check lets exactly these through.
    pub raw: Option<std::sync::Arc<str>>,
}

impl Line {
    pub fn new(spans: Vec<Span>) -> Self {
        Self { spans, fill_bg: None, raw: None }
    }
    pub fn empty() -> Self {
        Self::default()
    }
    pub fn raw(text: impl Into<String>) -> Self {
        Self::new(vec![Span::raw(text)])
    }
    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self::new(vec![Span::styled(text, style)])
    }
    pub fn push(&mut self, span: Span) {
        if span.text.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut()
            && last.style == span.style
        {
            last.text.push_str(&span.text);
            return;
        }
        self.spans.push(span);
    }
    pub fn push_str(&mut self, text: &str, style: Style) {
        self.push(Span::styled(text, style));
    }
    pub fn with_fill(mut self, c: Color) -> Self {
        self.fill_bg = Some(c);
        self
    }
    pub fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }
    pub fn plain(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
    /// Prepend spans (e.g. a gutter / indent).
    pub fn prefixed(mut self, prefix: Vec<Span>) -> Self {
        let width: usize = prefix.iter().map(Span::width).sum();
        if width > 0
            && let Some(raw) = &self.raw
            && let Some(rest) = raw.strip_prefix('\r')
        {
            // Core image commands restore column one before drawing. Preserve the layout's
            // accumulated gutter/list/quote indentation without changing the image payload.
            self.raw = Some(format!("\r\x1b[{width}C{rest}").into());
        }
        let mut spans = prefix;
        spans.append(&mut self.spans);
        self.spans = spans;
        self
    }
    /// Apply a base style under every span (span fields win).
    pub fn base_style(mut self, base: Style) -> Self {
        for s in &mut self.spans {
            s.style = base.patch(s.style);
        }
        self
    }
    /// Materialize deferred theme styles, fill background, and OSC 8 link capability against `theme`.
    pub fn resolve(&self, theme: &Theme) -> Self {
        Self {
            spans: self.spans.iter().map(|s| s.resolve(theme)).collect(),
            fill_bg: self.fill_bg.map(|c| c.resolve(theme)),
            raw: self.raw.clone(),
        }
    }
}
