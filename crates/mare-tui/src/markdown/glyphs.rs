//! Markdown symbols: the Unicode defaults, the Nerd Font and ASCII replacements, and the
//! lookup [`Renderer::glyph`] that picks one by key.
//!
//! ASCII-safe Markdown symbols for `icons = "ascii"`: a terminal that draws
//! emoji / ambiguous symbols one cell wide (`emoji_wide = false`, e.g. Terminal.app) selects the
//! ASCII icon set, and Markdown follows it with these in place of the Unicode defaults.
//! `mare.o.markdown.glyphs` overrides still win.

use super::*;

/// Default symbols (plain Unicode any font has); `mare.o.markdown.glyphs` overrides by key.
const GLYPHS: &[(&str, &str)] = &[
    ("bullet1", "•"),
    ("bullet2", "◦"),
    ("bullet3", "▪"),
    ("rule", "─"),
    ("quote", "▎"),
    ("code_top", "▄"),
    ("code_bottom", "▀"),
    ("streaming", "⋯"),
    ("task_done", "[✓]"),
    ("task_todo", "[ ]"),
    ("table_rule", "━"),
    ("note", "ⓘ"),
    ("tip", "✓"),
    ("important", "!"),
    ("warning", "⚠"),
    ("caution", "✕"),
];

/// Nerd Font replacements when `mare.o.icons = "nerd"` (render-markdown's callout icons).
const NERD: &[(&str, &str)] = &[
    ("note", "\u{f05a}"),
    ("tip", "\u{f0335}"),
    ("important", "\u{f017e}"),
    ("warning", "\u{f002a}"),
    ("caution", "\u{f0ce6}"),
];

/// Nerd Font language icons for fenced code block headers when `icons = "nerd"`.
const NERD_CODE_LANGS: &[(&str, &str)] = &[
    ("bash", "\u{f489}"),
    ("c", "\u{e61e}"),
    ("c#", "\u{e7b2}"),
    ("c++", "\u{e61d}"),
    ("cc", "\u{e61d}"),
    ("cpp", "\u{e61d}"),
    ("cs", "\u{e7b2}"),
    ("csharp", "\u{e7b2}"),
    ("css", "\u{e749}"),
    ("cxx", "\u{e61d}"),
    ("diff", "\u{f440}"),
    ("docker", "\u{f308}"),
    ("dockerfile", "\u{f308}"),
    ("elixir", "\u{e62d}"),
    ("ex", "\u{e62d}"),
    ("fish", "\u{f489}"),
    ("git", "\u{e702}"),
    ("gitcommit", "\u{e702}"),
    ("go", "\u{e627}"),
    ("golang", "\u{e627}"),
    ("haskell", "\u{e777}"),
    ("hs", "\u{e777}"),
    ("html", "\u{e736}"),
    ("java", "\u{e738}"),
    ("javascript", "\u{e74e}"),
    ("js", "\u{e74e}"),
    ("json", "\u{e60b}"),
    ("json5", "\u{e60b}"),
    ("jsonc", "\u{e60b}"),
    ("jsx", "\u{e7ba}"),
    ("kotlin", "\u{e634}"),
    ("kt", "\u{e634}"),
    ("lua", "\u{e620}"),
    ("make", "\u{e673}"),
    ("makefile", "\u{e673}"),
    ("markdown", "\u{e73e}"),
    ("md", "\u{e73e}"),
    ("ml", "\u{e67a}"),
    ("mysql", "\u{e706}"),
    ("nix", "\u{f313}"),
    ("ocaml", "\u{e67a}"),
    ("patch", "\u{f440}"),
    ("php", "\u{e73d}"),
    ("postgres", "\u{e706}"),
    ("postgresql", "\u{e706}"),
    ("powershell", "\u{e7ad}"),
    ("ps1", "\u{e7ad}"),
    ("pwsh", "\u{e7ad}"),
    ("py", "\u{e73c}"),
    ("python", "\u{e73c}"),
    ("rb", "\u{e739}"),
    ("rs", "\u{e7a8}"),
    ("ruby", "\u{e739}"),
    ("rust", "\u{e7a8}"),
    ("sass", "\u{e74b}"),
    ("scala", "\u{e737}"),
    ("scss", "\u{e74b}"),
    ("sh", "\u{f489}"),
    ("shell", "\u{f489}"),
    ("sql", "\u{e706}"),
    ("sqlite", "\u{e706}"),
    ("swift", "\u{e755}"),
    ("toml", "\u{e6b2}"),
    ("ts", "\u{e628}"),
    ("tsx", "\u{e7ba}"),
    ("typescript", "\u{e628}"),
    ("vim", "\u{e62b}"),
    ("vimscript", "\u{e62b}"),
    ("xml", "\u{e619}"),
    ("yaml", "\u{e6a8}"),
    ("yml", "\u{e6a8}"),
    ("zig", "\u{e6a9}"),
    ("zsh", "\u{f489}"),
];

pub(super) fn code_lang_nerd_icon(lang: &str) -> Option<&'static str> {
    let key = lang.trim();
    if key.is_empty() {
        return None;
    }
    let lower = key.to_ascii_lowercase();
    NERD_CODE_LANGS.iter().find(|(name, _)| *name == lower).map(|(_, icon)| *icon)
}

/// Replacements for the default glyphs, by key.
pub const ASCII_GLYPHS: &[(&str, &str)] = &[
    ("bullet1", "*"),
    ("bullet2", "-"),
    ("bullet3", "+"),
    ("rule", "-"),
    ("quote", "|"),
    ("code_top", "_"),
    ("code_bottom", "-"),
    ("streaming", "..."),
    ("task_done", "[x]"),
    ("task_todo", "[ ]"),
    ("table_rule", "-"),
    ("note", "i"),
    ("tip", "*"),
    ("important", "!"),
    ("warning", "!"),
    ("caution", "x"),
];

pub(super) fn theme_glyph(theme: &Theme, key: &str) -> String {
    if let Some(g) = theme.md.glyphs.get(key) {
        return g.clone();
    }
    if theme.icons == "nerd"
        && let Some((_, g)) = NERD.iter().find(|(k, _)| *k == key)
    {
        return g.to_string();
    }
    // emoji-ambiguous terminals (`emoji_wide = false`) select icons = "ascii"
    if theme.icons == "ascii"
        && let Some((_, g)) = ASCII_GLYPHS.iter().find(|(k, _)| *k == key)
    {
        return g.to_string();
    }
    GLYPHS.iter().find(|(k, _)| *k == key).map(|(_, g)| g.to_string()).unwrap_or_default()
}

impl Renderer<'_> {
    pub(super) fn glyph(&self, key: &str) -> String {
        theme_glyph(self.theme, key)
    }
}
