//! The theme: semantic color slots, named styles with their defaults, Markdown options.

use super::*;

/// Semantic color slots. Plugins refer to colors by these names.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    /// Validated tmTheme document selected by the public UI theme API.
    pub syntax: Option<String>,
    /// Named styles (Neovim highlight-group style): `MareAccent`, `MareToolName`, ...
    pub styles: HashMap<String, Style>,
    /// border charset for panels and popups: rounded | single | double | thick | hidden
    pub border: String,
    /// icon set: nerd | unicode | ascii
    pub icons: String,
    /// spinner glyphs, one per animation step (`mare.o.spinner`; default braille)
    pub spinner: Vec<String>,
    /// the marker drawn instead of cycling under `reduce_motion`
    pub spinner_still: String,
    /// Markdown rendering options: glyph overrides, line numbers, OSC 8 links
    pub md: MdConfig,
}

/// The built-in spinner (what `mare.o.spinner` unset draws) and its `reduce_motion` marker.
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const SPINNER_STILL: &str = "•";
pub const ASCII_SPINNER_FRAMES: &[&str] = &["-", "\\", "|", "/"];
pub const ASCII_SPINNER_STILL: &str = "*";

/// Markdown options from `mare.o.markdown` plus what the terminal supports (OSC 8).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MdConfig {
    /// `mare.o.markdown.glyphs` overrides by name (`bullets`, `rule`, `code_top`, …)
    pub glyphs: HashMap<String, String>,
    /// `mare.o.markdown.code_line_numbers` (default off)
    pub code_line_numbers: bool,
    /// links show only their text (the URL goes into OSC 8) when the terminal supports it
    pub osc8: bool,
}

/// Short slot names used by the core renderers → named styles.
const ALIASES: &[(&str, &str)] = &[
    ("text", "MareText"),
    ("accent", "MareAccent"),
    ("muted", "MareMuted"),
    ("dim", "MareDim"),
    ("subtle", "MareSubtle"),
    ("border", "MareBorder"),
    ("error", "MareError"),
    ("warning", "MareWarning"),
    ("success", "MareSuccess"),
    ("info", "MareInfo"),
    ("user_bg", "MareUserMessage"),
    ("code_bg", "MareCodeBlock"),
    ("inline_code", "MareInlineCode"),
    ("link", "MareLink"),
    ("heading", "MareHeading"),
    ("quote", "MareQuote"),
    ("diff_add_bg", "MareDiffAdd"),
    ("diff_del_bg", "MareDiffDelete"),
    ("diff_add_fg", "MareDiffAddText"),
    ("diff_del_fg", "MareDiffDeleteText"),
    ("diff_add_word", "MareDiffAddWord"),
    ("diff_del_word", "MareDiffDeleteWord"),
    ("selection_bg", "MareSelection"),
];

impl Default for Theme {
    fn default() -> Self {
        // The `theme-default` plugin's dark flavor (its `COLORS.dark` through `palette()`,
        // preset/plugins/theme-default.lua) before any background is measured: the frame drawn
        // before Lua runs, the startup-failure dialog and a session with theme-default turned off
        // look the same as the default theme (render test `the_rust_fallback_palette_matches_…`).
        let hex = |h: u32| Color::Rgb((h >> 16) as u8, (h >> 8) as u8, h as u8);
        let accent = hex(0xa0a4ff);
        let accent2 = hex(0xffa38f);
        let text = hex(0xcdd6f4);
        let muted = hex(0xb2b4d4);
        let dim = hex(0x8f91b1);
        let subtle = hex(0x707388);
        let border = hex(0x707199);
        let selection = hex(0x3f4062);
        let code_bg = hex(0x181825);
        let (green, yellow, red, blue) = (hex(0x9cd8b0), hex(0xf0d38f), hex(0xf57fa0), hex(0x85c1f5));
        let (add_bg, del_bg, add_word, del_word) = (hex(0x323c43), hex(0x402e40), hex(0x465a58), hex(0x633d52));
        let on_color = hex(0x141626);
        let fg = |c| Style::new().fg(c);
        let bg = |c| Style::new().bg(c);
        let mut styles = HashMap::new();
        let mut set = |k: &str, st: Style| {
            styles.insert(k.to_string(), st);
        };
        set("MareText", Style::new());
        set("MareAccent", fg(accent));
        set("MareMuted", fg(muted));
        set("MareDim", fg(dim));
        set("MareSubtle", fg(subtle));
        set("MareBorder", fg(border));
        set("MareError", fg(red));
        set("MareWarning", fg(yellow));
        set("MareSuccess", fg(green));
        set("MareInfo", fg(blue));
        // the user's message: a full-width bar in the selection color, default text color
        set("MareUserMessage", bg(selection));
        set("MareUserPrompt", fg(accent).bg(selection));
        set("MareAssistantMarker", Style::new());
        set("MareReasoning", fg(dim).italic());
        set("MareToolName", Style::new().bold());
        set("MareToolArgs", Style::new());
        set("MareToolOk", fg(green));
        set("MareToolError", fg(red));
        set("MareToolPending", fg(accent));
        set("MareToolResult", fg(muted));
        set("MareToolGutter", fg(dim));
        set("MareCodeBlock", bg(code_bg));
        set("MareInlineCode", fg(accent2));
        set("MareLink", fg(blue));
        set("MareHeading", fg(accent).bold());
        set("MareQuote", fg(muted));
        set("MareDiffAdd", bg(add_bg));
        set("MareDiffDelete", bg(del_bg));
        set("MareDiffAddText", fg(green));
        set("MareDiffDeleteText", fg(red));
        set("MareDiffAddWord", bg(add_word));
        set("MareDiffDeleteWord", bg(del_word));
        set("MareSelection", bg(selection));
        set("MareSpinner", fg(accent));
        set("MareStatus", fg(muted));
        set("MareStatusDim", fg(dim));
        set("MarePopupTitle", fg(accent).bold());
        set("MarePopupSelected", fg(accent).bold());
        set("MareHint", fg(dim));
        // interface shell
        set("MareBorderActive", fg(accent).bold());
        set("MareBorderSearch", fg(accent2).bold());
        set("MareTitle", fg(accent).bold());
        set("MareSelectionInactive", Style::new().bold());
        set("MareKeyHint", fg(accent));
        // permission preset capsules: READ / AUTO / FULL / CUSTOM
        set("MareModeReadOnly", fg(on_color).bg(green).bold());
        set("MareModeAuto", fg(on_color).bg(blue).bold());
        set("MareModeFull", fg(on_color).bg(red).bold());
        set("MareModeCustom", fg(on_color).bg(yellow).bold());
        set("MareNotifyInfo", fg(green).bold());
        set("MareNotifyWarn", fg(yellow).bold());
        set("MareNotifyError", fg(red).bold());
        set("MareScrollbar", fg(muted));
        // Markdown elements
        set("md.h1", fg(accent).bold()); // no underline: it read as a link
        set("md.h2", fg(accent).bold());
        set("md.h3", fg(text).bold());
        for h in ["md.h4", "md.h5", "md.h6"] {
            set(h, fg(muted).bold());
        }
        set("md.code", fg(accent2).bg(code_bg));
        set("md.code_block", bg(code_bg));
        set("md.code_edge", fg(code_bg));
        set("md.code_lang", fg(dim));
        set("md.code_line_nr", fg(dim));
        set("md.bullet", fg(accent));
        set("md.number", fg(accent));
        set("md.task_done", fg(green));
        set("md.task_todo", fg(dim));
        set("md.quote", fg(muted));
        set("md.quote_bar", fg(border));
        set("md.alert_note", fg(blue));
        set("md.alert_tip", fg(green));
        set("md.alert_important", fg(accent));
        set("md.alert_warning", fg(yellow));
        set("md.alert_caution", fg(red));
        set("md.table_header", Style::new().bold());
        set("md.table_rule", fg(border));
        set("md.rule", fg(border));
        set("md.link", fg(blue).underline());
        set("md.link_url", fg(dim));
        set("md.path", fg(accent2).bg(code_bg));
        set("md.footnote", fg(dim));
        set("md.html", fg(dim));
        set("md.strike", fg(dim));
        set("md.image", fg(dim));
        Self {
            syntax: None,
            styles,
            border: "rounded".into(),
            icons: "unicode".into(),
            spinner: SPINNER_FRAMES.iter().map(|g| g.to_string()).collect(),
            spinner_still: SPINNER_STILL.into(),
            md: MdConfig::default(),
        }
    }
}

impl Theme {
    fn canonical<'a>(&self, name: &'a str) -> &'a str {
        ALIASES.iter().find(|(a, _)| *a == name).map(|(_, m)| *m).unwrap_or(name)
    }
    /// The spinner glyph for an animation frame; `still` is the no-motion frame.
    pub fn spinner_glyph(&self, frame: usize, still: bool) -> &str {
        if still || self.spinner.is_empty() {
            if self.icons == "ascii" && self.spinner_still == SPINNER_STILL {
                ASCII_SPINNER_STILL
            } else {
                &self.spinner_still
            }
        } else if self.icons == "ascii" && self.spinner.iter().map(String::as_str).eq(SPINNER_FRAMES.iter().copied()) {
            ASCII_SPINNER_FRAMES[frame % ASCII_SPINNER_FRAMES.len()]
        } else {
            &self.spinner[frame % self.spinner.len()]
        }
    }
    /// Full named style (`MareXxx` or a short alias); unknown names parse as a color.
    pub fn style(&self, name: &str) -> Style {
        if let Some(st) = self.styles.get(self.canonical(name)) {
            return *st;
        }
        Color::parse(name).map(|c| Style::new().fg(c)).unwrap_or_default()
    }
    /// The color of a named style (its fg, or its bg for background-only styles).
    pub fn get(&self, name: &str) -> Color {
        let st = self.style(name);
        st.fg.or(st.bg).unwrap_or(Color::Reset)
    }
    /// Style for text in this named style.
    pub fn fg(&self, name: &str) -> Style {
        self.style(name)
    }
    /// Set the foreground color of a named style (theme helper).
    pub fn set(&mut self, name: &str, c: Color) {
        let key = self.canonical(name).to_string();
        let e = self.styles.entry(key).or_default();
        if e.fg.is_none() && e.bg.is_some() {
            e.bg = Some(c);
        } else {
            e.fg = Some(c);
        }
    }
    pub fn set_style(&mut self, name: &str, st: Style) {
        let key = self.canonical(name).to_string();
        self.styles.insert(key, st);
    }
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.styles.keys().cloned().collect();
        v.sort();
        v
    }
}
