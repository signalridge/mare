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
    let min = r.min(g).min(b);
    let delta = max - min;

    // Achromatic / near-neutral grays map strictly by perceptual luminance so dark-theme
    // pastels keep their chromatic hue while light-theme dark grays stay dark (never white).
    if delta < 44 && (max < 48 || u16::from(delta) * 3 < u16::from(max)) {
        return if lum < 56 {
            0
        } else if lum < 136 {
            8
        } else if lum < 204 {
            7
        } else {
            15
        };
    }

    let d = i32::from(delta);
    let (ri, gi, bi) = (i32::from(r), i32::from(g), i32::from(b));
    let hue = if max == r {
        (60 * (gi - bi) / d).rem_euclid(360)
    } else if max == g {
        120 + 60 * (bi - ri) / d
    } else {
        240 + 60 * (ri - gi) / d
    };
    let base = if !(30..330).contains(&hue) {
        1 // red
    } else if (30..90).contains(&hue) {
        3 // yellow
    } else if (90..155).contains(&hue) {
        2 // green
    } else if (155..200).contains(&hue) {
        6 // cyan
    } else if (200..270).contains(&hue) {
        4 // blue
    } else {
        5 // magenta
    };
    if lum > 140 || (max >= 240 && min < 64) { base + 8 } else { base }
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

#[cfg(test)]
mod repro_213_tests {
    use super::*;

    #[test]
    fn repro_213_ansi16_fallback_preserves_contrast_for_default_palettes() {
        // Default dark theme colors from lua/theme-default/defaults.lua:125-138
        let dark_text = rgb_to_16(0xcd, 0xd6, 0xf4);
        let dark_accents = [
            ("accent", rgb_to_16(0xa0, 0xa4, 0xff)),
            ("accent2", rgb_to_16(0xff, 0xa3, 0x8f)),
            ("green", rgb_to_16(0x9c, 0xd8, 0xb0)),
            ("yellow", rgb_to_16(0xf0, 0xd3, 0x8f)),
            ("red", rgb_to_16(0xf5, 0x7f, 0xa0)),
            ("blue", rgb_to_16(0x85, 0xc1, 0xf5)),
        ];
        // Default light theme body text from lua/theme-default/defaults.lua:148
        let light_text = rgb_to_16(0x44, 0x47, 0x5f);

        let distinct_from_text: Vec<_> = dark_accents.iter().filter(|(_, code)| *code != dark_text).collect();
        assert!(
            !distinct_from_text.is_empty(),
            "all dark theme semantic accents collapsed to body text ANSI code {dark_text}: {dark_accents:?}; light text mapped to ANSI code {light_text}"
        );
        assert!(
            light_text != 7 && light_text != 15,
            "light theme body text #44475f must not map to ANSI white/bright-white (got {light_text})"
        );

        // Every dark-theme semantic accent must keep a chromatic ANSI color (not white/gray).
        for (name, code) in dark_accents {
            assert!(
                (9..=14).contains(&code),
                "dark theme accent {name} should map to a bright chromatic ANSI code (9..=14), got {code}"
            );
        }
        // Light-theme accents should map to darker normal chromatic ANSI codes (1..=6) for contrast on light bg.
        for (name, (r, g, b)) in [
            ("accent", (0x55, 0x58, 0xd6)),
            ("accent2", (0xd8, 0x60, 0x4c)),
            ("green", (0x2f, 0x8a, 0x5c)),
            ("yellow", (0xa0, 0x78, 0x0f)),
            ("red", (0xc8, 0x40, 0x6a)),
            ("blue", (0x26, 0x75, 0xc4)),
        ] {
            let code = rgb_to_16(r, g, b);
            assert!(
                (1..=6).contains(&code),
                "light theme accent {name} should map to a normal chromatic ANSI code (1..=6), got {code}"
            );
        }
        // NoColor must still emit no color SGR at all (including bright 90..97).
        let no_color = style_sgr(Style::new().fg(Color::Rgb(0xa0, 0xa4, 0xff)), ColorMode::NoColor);
        assert_eq!(no_color, "\x1b[0m");
    }
}
