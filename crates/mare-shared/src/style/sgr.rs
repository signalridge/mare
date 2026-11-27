//! ANSI encoding: color mode detection, SGR sequences and a line as an ANSI string.

use super::model::link_url;
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    TrueColor,
    Ansi256,
    Ansi16,
    /// `NO_COLOR`: no color at all; bold, underline and reverse video
    /// stay (reverse marks the selected row / button, which otherwise is only a background)
    NoColor,
}

/// `NO_COLOR` is set to a non-empty value (no-color.org).
pub fn no_color_env(var: &dyn Fn(&str) -> Option<String>) -> bool {
    var("NO_COLOR").is_some_and(|v| !v.is_empty())
}

impl ColorMode {
    /// Parse an explicit color-depth setting (`"truecolor" | "256" | "16" | "none"`).
    /// `"auto"` returns `None` so env/probe detection proceeds.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "truecolor" => Some(Self::TrueColor),
            "256" => Some(Self::Ansi256),
            "16" | "ansi16" => Some(Self::Ansi16),
            "none" | "no_color" => Some(Self::NoColor),
            _ => None,
        }
    }

    /// Canonical string representation shared with Lua (`mare.ui.color_depth()`, `mare.ui.caps()`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TrueColor => "truecolor",
            Self::Ansi256 => "256",
            Self::Ansi16 => "16",
            Self::NoColor => "none",
        }
    }

    /// Resolve terminal color depth through one canonical hierarchy:
    /// explicit `option` (`terminal.color != "auto"`) > env (`NO_COLOR`, `MARE_COLOR`,
    /// `COLORTERM`) > probe (`probed_truecolor`) > terminal defaults (`TERM`, `TERM_PROGRAM`).
    pub fn resolve(option: Option<&str>, var: &dyn Fn(&str) -> Option<String>, probed_truecolor: Option<bool>) -> Self {
        if let Some(m) = option.and_then(Self::parse) {
            return m;
        }
        if no_color_env(var) {
            return Self::NoColor;
        }
        if let Some(m) = var("MARE_COLOR").as_deref().and_then(Self::parse) {
            return m;
        }
        if matches!(var("COLORTERM").as_deref(), Some("truecolor" | "24bit")) {
            return Self::TrueColor;
        }
        if let Some(tc) = probed_truecolor {
            return if tc { Self::TrueColor } else { Self::Ansi256 };
        }
        let non_empty = |k: &str| var(k).is_some_and(|v| !v.is_empty());
        if non_empty("WT_SESSION")
            || non_empty("KITTY_WINDOW_ID")
            || non_empty("GHOSTTY_RESOURCES_DIR")
            || non_empty("WEZTERM_PANE")
            || non_empty("WEZTERM_EXECUTABLE")
            || non_empty("ITERM_SESSION_ID")
        {
            return Self::TrueColor;
        }
        let term = var("TERM").unwrap_or_default();
        match var("TERM_PROGRAM").as_deref() {
            Some("Apple_Terminal") => return Self::Ansi256,
            Some(p) if !p.is_empty() => return Self::TrueColor,
            _ => {}
        }
        if matches!(term.as_str(), "xterm-kitty" | "alacritty" | "wezterm")
            || term.starts_with("foot")
            || term.contains("direct")
        {
            return Self::TrueColor;
        }
        if term.contains("256") {
            return Self::Ansi256;
        }
        if matches!(term.as_str(), "" | "dumb" | "linux" | "vt100" | "ansi" | "xterm") {
            Self::Ansi16
        } else {
            Self::Ansi256
        }
    }

    pub fn detect() -> Self {
        Self::resolve(None, &|k| std::env::var(k).ok(), None)
    }
}

/// Resolve the active icon set (`"nerd" | "unicode" | "ascii"`) through one canonical hierarchy:
/// explicit `option` (`icons != "auto"`) > env (`MARE_NERD_FONT`, `NERD_FONT`, locale + `TERM`)
/// > capability probe/table (`emoji_wide`) > terminal heuristic default.
pub fn resolve_icons(
    option: Option<&str>,
    var: &dyn Fn(&str) -> Option<String>,
    emoji_wide: Option<bool>,
) -> &'static str {
    match option {
        Some("nerd") => return "nerd",
        Some("unicode") => return "unicode",
        Some("ascii") => return "ascii",
        _ => {}
    }
    if var("MARE_NERD_FONT").as_deref() == Some("1") || var("NERD_FONT").as_deref() == Some("1") {
        return "nerd";
    }
    let locale =
        var("LC_ALL").or_else(|| var("LC_CTYPE")).or_else(|| var("LANG")).unwrap_or_default().to_ascii_uppercase();
    if !locale.contains("UTF-8") && !locale.contains("UTF8") && var("TERM").as_deref() == Some("linux") {
        return "ascii";
    }
    if let Some(wide) = emoji_wide {
        return if wide { "unicode" } else { "ascii" };
    }
    if var("TERM_PROGRAM").as_deref() == Some("Apple_Terminal") {
        let ver = var("TERM_PROGRAM_VERSION")
            .unwrap_or_default()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(0);
        if ver < 460 {
            return "ascii";
        }
    }
    "unicode"
}

fn rgb_to_16(r: u8, g: u8, b: u8) -> u8 {
    let lum = (u16::from(r) * 30 + u16::from(g) * 59 + u16::from(b) * 11) / 100;
    let max = r.max(g).max(b);
    let thresh = (max / 2).max(64);
    let code = u8::from(r >= thresh) | (u8::from(g >= thresh) << 1) | (u8::from(b >= thresh) << 2);
    if lum > 140 { code + 8 } else { code }
}

fn ansi256_to_16(n: u8) -> u8 {
    if n < 16 {
        n
    } else if n >= 232 {
        let g = (n - 232) * 10 + 8;
        if g < 64 {
            0
        } else if g < 140 {
            8
        } else if g < 200 {
            7
        } else {
            15
        }
    } else {
        let idx = n - 16;
        let level = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
        rgb_to_16(level(idx / 36), level((idx / 6) % 6), level(idx % 6))
    }
}

fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    // grayscale ramp
    if r == g && g == b {
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return 232 + ((r as u16 - 8) * 24 / 247) as u8;
    }
    let q = |v: u8| -> u8 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            ((v as u16 - 35) / 40) as u8
        }
    };
    16 + 36 * q(r) + 6 * q(g) + q(b)
}

fn push_color(out: &mut String, c: Color, bg: bool, mode: ColorMode) {
    if mode == ColorMode::NoColor {
        return;
    }
    let base = if bg { 48 } else { 38 };
    match c {
        Color::Reset => {
            let _ = write!(out, "\x1b[{}m", if bg { 49 } else { 39 });
        }
        Color::Theme(_) => {}
        Color::Ansi(n) if n < 8 => {
            let _ = write!(out, "\x1b[{}m", (if bg { 40 } else { 30 }) + n as u16);
        }
        Color::Ansi(n) if n < 16 => {
            let _ = write!(out, "\x1b[{}m", (if bg { 100 } else { 90 }) + (n - 8) as u16);
        }
        Color::Ansi(n) => {
            if mode == ColorMode::Ansi16 {
                let c16 = ansi256_to_16(n);
                let code = if c16 < 8 {
                    (if bg { 40 } else { 30 }) + c16 as u16
                } else {
                    (if bg { 100 } else { 90 }) + (c16 - 8) as u16
                };
                let _ = write!(out, "\x1b[{code}m");
            } else {
                let _ = write!(out, "\x1b[{base};5;{n}m");
            }
        }
        Color::Rgb(r, g, b) => match mode {
            ColorMode::TrueColor => {
                let _ = write!(out, "\x1b[{base};2;{r};{g};{b}m");
            }
            ColorMode::Ansi256 => {
                let _ = write!(out, "\x1b[{base};5;{}m", rgb_to_256(r, g, b));
            }
            ColorMode::Ansi16 => {
                let c16 = rgb_to_16(r, g, b);
                let code = if c16 < 8 {
                    (if bg { 40 } else { 30 }) + c16 as u16
                } else {
                    (if bg { 100 } else { 90 }) + (c16 - 8) as u16
                };
                let _ = write!(out, "\x1b[{code}m");
            }
            ColorMode::NoColor => {}
        },
    }
}

pub fn style_sgr(st: Style, mode: ColorMode) -> String {
    let mut out = String::from("\x1b[0m");
    let color = mode != ColorMode::NoColor;
    if st.bold {
        out.push_str("\x1b[1m");
    }
    if st.dim && color {
        out.push_str("\x1b[2m");
    }
    if st.italic && color {
        out.push_str("\x1b[3m");
    }
    if st.underline {
        out.push_str("\x1b[4m");
    }
    if st.reverse {
        out.push_str("\x1b[7m");
    }
    if st.strike && color {
        out.push_str("\x1b[9m");
    }
    if let Some(c) = st.fg {
        push_color(&mut out, c, false, mode);
    }
    if let Some(c) = st.bg {
        push_color(&mut out, c, true, mode);
    }
    out
}

/// Encode a line as an ANSI string, clipped to `width` cells. Ends with a reset.
pub fn line_to_ansi(line: &Line, width: usize, mode: ColorMode) -> String {
    let line = if line.width() > width { truncate_line(line, width) } else { line.clone() };
    let mut out = String::new();
    let mut last: Option<Style> = None;
    for span in &line.spans {
        if span.text.is_empty() {
            continue;
        }
        if last != Some(span.style) {
            let prev_link = last.map_or(0, |l| l.link);
            if prev_link != span.style.link {
                // OSC 8 hyperlinks: close the previous target, open the new one
                if prev_link != 0 {
                    out.push_str("\x1b]8;;\x1b\\");
                }
                if let Some(url) = link_url(span.style.link) {
                    out.push_str("\x1b]8;;");
                    out.extend(url.chars().filter(|c| !c.is_control()));
                    out.push_str("\x1b\\");
                }
            }
            out.push_str(&style_sgr(span.style, mode));
            last = Some(span.style);
        }
        // span text is plain text: control characters are never encoded (spans are cleaned
        // when they're built; the counted final check runs at the write exit, mare_tui::out)
        if span.text.chars().any(|c| c.is_control()) {
            out.extend(span.text.chars().filter(|c| !c.is_control()));
        } else {
            out.push_str(&span.text);
        }
    }
    if last.is_some_and(|l| l.link != 0) {
        out.push_str("\x1b]8;;\x1b\\");
    }
    if let Some(bg) = line.fill_bg {
        let w = line.width();
        if w < width {
            let mut st = Style::new();
            st.bg = Some(bg);
            out.push_str(&style_sgr(st, mode));
            out.extend(std::iter::repeat_n(' ', width - w));
        }
    }
    out.push_str("\x1b[0m");
    if let Some(raw) = &line.raw {
        out.push_str(raw);
    }
    out
}
