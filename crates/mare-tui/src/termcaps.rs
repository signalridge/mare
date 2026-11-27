//! Static terminal capability table: what each known terminal supports,
//! looked up from environment variables before the startup probe confirms it (MIT). `terminal_caps` in init.lua
//! and the `ui/caps` event still override what this table says.

use crate::style::WidthMethod;

/// What a terminal is known to support.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermCaps {
    /// display name
    pub name: &'static str,
    pub truecolor: bool,
    /// emoji drawn two cells wide (Terminal.app before macOS Tahoe draws many one cell wide)
    pub emoji_wide: bool,
    pub kitty_keyboard: bool,
    pub bracketed_paste: bool,
    /// draws a grapheme cluster (ZWJ sequence, VS16 emoji, flag) as one unit: the width rule is
    /// [`WidthMethod::Unicode`]; otherwise it adds up code points ([`WidthMethod::Wcwidth`])
    pub graphemes: bool,
    /// the terminal's known rendering quirk, if any
    pub quirk: Option<&'static str>,
}

const fn caps(
    name: &'static str,
    truecolor: bool,
    emoji_wide: bool,
    kitty: bool,
    quirk: Option<&'static str>,
) -> TermCaps {
    TermCaps { name, truecolor, emoji_wide, kitty_keyboard: kitty, bracketed_paste: true, graphemes: true, quirk }
}

/// A terminal that measures code point by code point (OpenTUI `terminal.zig` sets `wcwidth`
/// for Terminal.app, tmux and screen; Alacritty, Konsole and VTE don't join clusters either).
const fn per_code_point(mut c: TermCaps) -> TermCaps {
    c.graphemes = false;
    c
}

/// Terminal.app versions from macOS Tahoe (26) on draw truecolor and wide emoji; earlier ones
/// clamp RGB to 256 colours and draw many emoji one cell wide. The version is
/// `TERM_PROGRAM_VERSION`; Tahoe's Terminal starts at 460.
const TAHOE_TERMINAL: u32 = 460;

/// The table entry for the terminal this process runs in (outer terminal first; a multiplexer
/// adds its own limits on top), or `None` when the terminal isn't known.
pub fn detect(var: &dyn Fn(&str) -> Option<String>) -> Option<TermCaps> {
    // an empty variable (e.g. `TMUX=` left behind) means unset
    let var = |k: &str| var(k).filter(|v| !v.is_empty());
    let prog = var("TERM_PROGRAM").unwrap_or_default();
    let term = var("TERM").unwrap_or_default();
    let mut c = match prog.as_str() {
        "Apple_Terminal" => {
            let v: u32 =
                var("TERM_PROGRAM_VERSION").and_then(|v| v.split('.').next().and_then(|m| m.parse().ok())).unwrap_or(0);
            if v >= TAHOE_TERMINAL {
                per_code_point(caps("Terminal.app", true, true, false, None))
            } else {
                per_code_point(caps(
                    "Terminal.app (before macOS Tahoe)",
                    false,
                    false,
                    false,
                    Some("no truecolor (RGB clamped to 256 colours); many emoji drawn one cell wide"),
                ))
            }
        }
        "iTerm.app" => caps("iTerm2", true, true, true, None),
        "ghostty" => caps("Ghostty", true, true, true, None),
        "WezTerm" => caps("WezTerm", true, true, true, None),
        "WarpTerminal" => {
            caps("Warp", true, true, false, Some("its block UI intercepts paste, mouse and the alternate screen"))
        }
        "vscode" => caps("VS Code terminal", true, true, true, Some("emoji width depends on the editor font")),
        _ => {
            if var("KITTY_WINDOW_ID").is_some() || term == "xterm-kitty" {
                caps("kitty", true, true, true, None)
            } else if var("GHOSTTY_RESOURCES_DIR").is_some() {
                caps("Ghostty", true, true, true, None)
            } else if var("WEZTERM_EXECUTABLE").is_some() || var("WEZTERM_PANE").is_some() {
                caps("WezTerm", true, true, true, None)
            } else if var("ALACRITTY_SOCKET").is_some() || var("ALACRITTY_WINDOW_ID").is_some() || term == "alacritty" {
                per_code_point(caps("Alacritty", true, true, true, None))
            } else if var("WT_SESSION").is_some() {
                caps("Windows Terminal", true, true, false, Some("ConPTY can drop rapid escape sequences"))
            } else if var("KONSOLE_VERSION").is_some() {
                per_code_point(caps(
                    "Konsole",
                    true,
                    true,
                    false,
                    Some("reflow on resize can briefly corrupt the display"),
                ))
            } else if var("VTE_VERSION").is_some() {
                per_code_point(caps("GNOME Terminal (VTE)", true, true, false, None))
            } else {
                return None;
            }
        }
    };
    // multiplexers sit in between: their own width tables and escape filtering
    if var("TMUX").is_some() {
        c.kitty_keyboard = false;
        c.graphemes = false;
        c.quirk = Some("tmux: own width table and escape filtering (truecolor needs terminal-features RGB)");
    } else if var("STY").is_some() {
        c.truecolor = false;
        c.emoji_wide = false;
        c.kitty_keyboard = false;
        c.graphemes = false;
        c.quirk = Some("screen: no truecolor, outdated width tables");
    }
    Some(c)
}

/// The width rule for this terminal ( 宽度和换行, after OpenTUI `terminal.zig`
/// 700-870): `MARE_WIDTH_METHOD=unicode|wcwidth` forces one; a terminal that reports mode 2027
/// (grapheme clustering) as set measures clusters; otherwise the capability table decides, and
/// an unknown terminal gets the conservative per-code-point rule (a cluster measured too wide
/// only leaves blank cells, one measured too narrow wraps the row). Multiplexers keep their
/// own width table whatever the outer terminal answers.
pub fn width_method(var: &dyn Fn(&str) -> Option<String>, mode_2027_set: Option<bool>) -> WidthMethod {
    if let Some(m) = var("MARE_WIDTH_METHOD").as_deref().and_then(WidthMethod::parse) {
        return m;
    }
    let mux = {
        let var = |k: &str| var(k).filter(|v| !v.is_empty());
        let term = var("TERM").unwrap_or_default();
        var("TMUX").is_some() || var("STY").is_some() || term.starts_with("tmux") || term.starts_with("screen")
    };
    if mode_2027_set == Some(true) && !mux {
        return WidthMethod::Unicode;
    }
    match detect(var) {
        Some(c) if c.graphemes && !mux => WidthMethod::Unicode,
        _ => WidthMethod::Wcwidth,
    }
}

/// Cells kept free at the end of every row ( 字符宽度: "tmux 和 screen 有自己的宽度表，
/// 行尾留余量"): a multiplexer measures characters with its own (often older) width table, so a
/// row the renderer thinks fills the line exactly can wrap there and shift the live area. One
/// cell absorbs a single disagreement (an emoji or an ambiguous-width glyph counted 1 by mare and
/// 2 by tmux). Zero elsewhere.
pub fn line_end_margin(var: &dyn Fn(&str) -> Option<String>) -> usize {
    let var = |k: &str| var(k).filter(|v| !v.is_empty());
    let term = var("TERM").unwrap_or_default();
    let tmux = var("TMUX").is_some() || term.starts_with("tmux");
    let screen = var("STY").is_some() || term.starts_with("screen");
    usize::from(tmux || screen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_method_follows_env_then_mode_2027_then_the_table() {
        let ghostty = [("TERM_PROGRAM", "ghostty")];
        assert_eq!(width_method(&env(&ghostty), None), WidthMethod::Unicode);
        assert_eq!(width_method(&env(&[("TERM_PROGRAM", "Apple_Terminal")]), None), WidthMethod::Wcwidth);
        assert_eq!(width_method(&env(&[("TERM", "xterm")]), None), WidthMethod::Wcwidth, "unknown: conservative");
        assert_eq!(width_method(&env(&[("TERM", "xterm")]), Some(true)), WidthMethod::Unicode, "2027 is set");
        let tmux = [("TERM_PROGRAM", "ghostty"), ("TMUX", "/tmp/x")];
        assert_eq!(width_method(&env(&tmux), Some(true)), WidthMethod::Wcwidth, "tmux measures itself");
        let forced = [("TERM_PROGRAM", "Apple_Terminal"), ("MARE_WIDTH_METHOD", "unicode")];
        assert_eq!(width_method(&env(&forced), Some(false)), WidthMethod::Unicode);
        let forced = [("TERM_PROGRAM", "ghostty"), ("MARE_WIDTH_METHOD", "wcwidth")];
        assert_eq!(width_method(&env(&forced), Some(true)), WidthMethod::Wcwidth);
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn terminal_app_before_tahoe_has_no_truecolor_and_narrow_emoji() {
        let old = detect(&env(&[("TERM_PROGRAM", "Apple_Terminal"), ("TERM_PROGRAM_VERSION", "455.1")])).unwrap();
        assert!(!old.truecolor && !old.emoji_wide && old.quirk.is_some(), "{old:?}");
        let new = detect(&env(&[("TERM_PROGRAM", "Apple_Terminal"), ("TERM_PROGRAM_VERSION", "466")])).unwrap();
        assert!(new.truecolor && new.emoji_wide, "{new:?}");
    }

    #[test]
    fn multiplexers_narrow_what_the_outer_terminal_offers() {
        let k = detect(&env(&[("TERM_PROGRAM", "ghostty"), ("TMUX", "/tmp/x")])).unwrap();
        assert!(!k.kitty_keyboard && k.quirk.unwrap().starts_with("tmux"));
        assert_eq!(detect(&env(&[("TERM", "xterm")])), None);
    }

    #[test]
    fn multiplexers_get_a_line_end_margin() {
        assert_eq!(line_end_margin(&env(&[("TERM_PROGRAM", "ghostty")])), 0);
        assert_eq!(line_end_margin(&env(&[("TMUX", "/tmp/tmux-1/default,1,0")])), 1);
        assert_eq!(line_end_margin(&env(&[("TERM", "screen-256color")])), 1);
        assert_eq!(line_end_margin(&env(&[("STY", "123.pts-0")])), 1);
        assert_eq!(line_end_margin(&env(&[("TMUX", "")])), 0, "an empty variable means unset");
    }
}
