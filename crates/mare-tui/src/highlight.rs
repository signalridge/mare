//! Syntax highlighting for code blocks and diffs (syntect, pure-Rust regex).

use std::sync::{Arc, OnceLock};

use rustc_hash::{FxHashMap, FxHashSet};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme as SynTheme};
use syntect::parsing::syntax_definition::{Context, ContextReference, MatchOperation, MatchPattern, Pattern};
use syntect::parsing::{Regex as SynRegex, SyntaxDefinition, SyntaxReference, SyntaxSet};

use super::style::*;

/// Curated grammars.
/// Regenerate with `cargo test gen_syntax_assets -- --ignored`.
const SYNTAX_DUMP: &[u8] = include_bytes!("../assets/syntaxes.packdump");
const THEME_DUMP: &[u8] = include_bytes!("../assets/theme.themedump");
/// The light flavor's syntax colors: `InspiredGitHub`, one of two_face's (bat's)
/// light themes, which syntect also bundles — dark-on-light colors with readable contrast on the
/// light `code_bg`. Regenerate with `cargo test gen_light_theme_asset -- --ignored`.
const LIGHT_THEME_DUMP: &[u8] = include_bytes!("../assets/theme-light.themedump");

/// Load the syntax set + theme ahead of time (called in the background at `ui/ready`), so the
/// first highlighted code block doesn't pay for deserialization.
pub fn preload() {
    let _ = syntax_set();
    let _ = default_syn_theme(false);
}

fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(|| syntect::dumps::from_uncompressed_data(SYNTAX_DUMP).expect("curated syntax dump"))
}

fn default_syn_theme(light: bool) -> Arc<SynTheme> {
    static TH: OnceLock<Arc<SynTheme>> = OnceLock::new();
    static LIGHT: OnceLock<Arc<SynTheme>> = OnceLock::new();
    if light {
        LIGHT.get_or_init(|| Arc::new(syntect::dumps::from_binary(LIGHT_THEME_DUMP))).clone()
    } else {
        TH.get_or_init(|| Arc::new(syntect::dumps::from_binary(THEME_DUMP))).clone()
    }
}

/// Whether the theme is a light flavor: code sits on a light card (`md.code_block`, else
/// `code_bg`), so syntax colors must be dark. Decided from the color itself, so any theme pack
/// (not only theme-default's 薄暮 Latte) gets the light syntax colors.
pub fn is_light(theme: &Theme) -> bool {
    let bg = theme.style("md.code_block").bg.or(theme.style("code_bg").bg);
    match bg {
        Some(Color::Rgb(r, g, b)) => {
            // relative luminance (sRGB weights, no gamma: enough to tell dark from light)
            0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32 > 140.0
        }
        _ => false,
    }
}

/// syntect's complete default grammar set (embedded compressed; loaded only for languages the
/// curated set doesn't cover).
fn full_syntax_set() -> &'static SyntaxSet {
    static FULL: OnceLock<SyntaxSet> = OnceLock::new();
    FULL.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn find_in_full(lang: &str) -> Option<&'static SyntaxReference> {
    let token = lang.trim().split(|c: char| c.is_whitespace() || c == ',' || c == '{').next()?.trim_start_matches('.');
    if token.is_empty() {
        return None;
    }
    let ss = full_syntax_set();
    ss.find_syntax_by_token(token).or_else(|| ss.find_syntax_by_extension(&token.to_ascii_lowercase()))
}

fn substitute_backrefs(pat: &str, non_empty_mask: u16) -> String {
    let mut out = String::with_capacity(pat.len());
    let mut esc = false;
    for c in pat.chars() {
        if esc && c.is_ascii_digit() {
            let d = c.to_digit(10).expect("ascii digit") as u16;
            if (non_empty_mask & (1 << d)) != 0 {
                out.push('a');
            }
        } else if esc {
            out.push('\\');
            out.push(c);
        } else if c != '\\' {
            out.push(c);
        }
        esc = c == '\\' && !esc;
    }
    if esc {
        out.push('\\');
    }
    out
}

fn pattern_compiles(m: &MatchPattern) -> bool {
    let raw = m.regex.regex_str();
    if !m.has_captures {
        return SynRegex::try_compile(raw).is_none();
    }
    let mut digits = Vec::new();
    let mut esc = false;
    for c in raw.chars() {
        if esc && c.is_ascii_digit() {
            let d = c.to_digit(10).expect("ascii digit") as u16;
            if !digits.contains(&d) {
                digits.push(d);
            }
        }
        esc = c == '\\' && !esc;
    }
    if digits.len() <= 4 {
        for bits in 0..(1u32 << digits.len()) {
            let mut mask = 0u16;
            for (bit, &d) in digits.iter().enumerate() {
                if (bits & (1 << bit)) != 0 {
                    mask |= 1 << d;
                }
            }
            if SynRegex::try_compile(&substitute_backrefs(raw, mask)).is_some() {
                return false;
            }
        }
        return true;
    }
    let all = digits.iter().fold(0u16, |acc, &d| acc | (1 << d));
    let mut masks = vec![0u16, all];
    for &d in &digits {
        masks.push(1 << d);
        masks.push(all & !(1 << d));
    }
    masks.into_iter().all(|mask| SynRegex::try_compile(&substitute_backrefs(raw, mask)).is_none())
}

fn syntax_patterns_valid<'a>(ss: &'a SyntaxSet, defs: &'a [SyntaxDefinition], idx: usize) -> bool {
    let Some(def) = defs.get(idx) else {
        return false;
    };
    if let Some(flm) = &def.first_line_match
        && SynRegex::try_compile(flm).is_some()
    {
        return false;
    }
    let mut stack: Vec<&'a Context> = def.contexts.values().collect();
    let mut seen_resolved = FxHashSet::default();
    fn enqueue<'a>(ss: &'a SyntaxSet, r: &ContextReference, stack: &mut Vec<&'a Context>, seen: &mut FxHashSet<usize>) {
        if let Ok(ctx) = r.resolve(ss)
            && seen.insert(ctx as *const Context as usize)
        {
            stack.push(ctx);
        }
    }
    while let Some(ctx) = stack.pop() {
        for pat in &ctx.patterns {
            match pat {
                Pattern::Match(m) => {
                    if !pattern_compiles(m) {
                        return false;
                    }
                    match &m.operation {
                        MatchOperation::Push(refs) | MatchOperation::Set(refs) => {
                            for r in refs {
                                enqueue(ss, r, &mut stack, &mut seen_resolved);
                            }
                        }
                        MatchOperation::Pop | MatchOperation::None => {}
                    }
                    if let Some(r) = &m.with_prototype {
                        enqueue(ss, r, &mut stack, &mut seen_resolved);
                    }
                }
                Pattern::Include(r) => enqueue(ss, r, &mut stack, &mut seen_resolved),
            }
        }
    }
    true
}

static VALIDATED_SYNTAXES: std::sync::Mutex<Option<FxHashMap<usize, bool>>> = std::sync::Mutex::new(None);
#[cfg(test)]
thread_local! {
    static TEST_SYNTAX_SET: std::cell::RefCell<Option<&'static SyntaxSet>> = const { std::cell::RefCell::new(None) };
}

fn is_syntax_valid(ss: &'static SyntaxSet, syntax: &'static SyntaxReference) -> bool {
    let key = syntax as *const SyntaxReference as usize;
    if let Some(&ok) = VALIDATED_SYNTAXES.lock().unwrap().as_ref().and_then(|m| m.get(&key)) {
        return ok;
    }
    let Some(idx) = ss.syntaxes().iter().position(|s| std::ptr::eq(s, syntax)) else {
        return false;
    };
    let defs = ss.clone().into_builder();
    let ok = syntax_patterns_valid(ss, defs.syntaxes(), idx);
    if !ok {
        tracing::warn!(
            syntax = syntax.name,
            scope = %syntax.scope,
            "disabling syntax with uncompilable regex; falling back to plain text"
        );
    }
    VALIDATED_SYNTAXES.lock().unwrap().get_or_insert_with(FxHashMap::default).insert(key, ok);
    ok
}

fn resolve_syntax(lang: &str) -> Option<(&'static SyntaxSet, &'static SyntaxReference)> {
    #[cfg(test)]
    if let Some(ss) = TEST_SYNTAX_SET.with(|slot| *slot.borrow()) {
        let token = lang.trim().split(|c: char| c.is_whitespace() || c == ',' || c == '{').next().unwrap_or("");
        if let Some(s) = ss.find_syntax_by_token(token) {
            return is_syntax_valid(ss, s).then_some((ss, s));
        }
    }
    let (ss, syntax) =
        find_syntax(lang).map(|s| (syntax_set(), s)).or_else(|| find_in_full(lang).map(|s| (full_syntax_set(), s)))?;
    is_syntax_valid(ss, syntax).then_some((ss, syntax))
}

/// Resolve a fence info string / extension / alias to a syntax.
pub fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let ss = syntax_set();
    let token = lang
        .trim()
        .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
        .next()
        .unwrap_or("")
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if token.is_empty() {
        return None;
    }
    let mapped: &[&str] = match token.as_str() {
        "rs" | "rust" => &["rs"],
        "py" | "python" | "python3" => &["py"],
        "ts" | "typescript" | "mts" | "cts" => &["ts", "js"],
        "tsx" => &["tsx", "ts", "js"],
        "js" | "javascript" | "jsx" | "mjs" | "cjs" => &["js"],
        "sh" | "bash" | "zsh" | "shell" | "console" | "shellscript" | "fish" => &["sh"],
        "json" | "jsonc" | "json5" => &["json"],
        "toml" => &["toml"],
        "ini" | "cfg" | "properties" => &["properties"],
        "yaml" | "yml" => &["yaml"],
        "go" | "golang" => &["go"],
        "c" | "h" => &["c"],
        "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hh" => &["cpp"],
        "md" | "markdown" => &["md"],
        "lua" | "luau" => &["lua"],
        "diff" | "patch" => &["diff"],
        "rb" | "ruby" => &["rb"],
        "java" => &["java"],
        "html" | "htm" | "xml" | "svg" => &["html", "xml"],
        "css" | "scss" => &["css"],
        "sql" => &["sql"],
        "make" | "makefile" => &["make"],
        _ => &[],
    };
    for ext in mapped {
        if let Some(s) = ss.find_syntax_by_extension(ext) {
            return Some(s);
        }
    }
    ss.find_syntax_by_token(&token).or_else(|| ss.find_syntax_by_extension(&token)).or_else(|| {
        // grammars without extensions (e.g. "Diff (Basic)"): match by scope
        let names = [format!("source.{token}"), format!("text.{token}")];
        let alias = if token == "patch" { "source.diff".to_string() } else { String::new() };
        ss.syntaxes().iter().find(|s| {
            let sc = s.scope.build_string();
            names.contains(&sc) || sc == alias
        })
    })
}

// Bat/syntect ANSI theme encoding: alpha 0 carries a palette index; alpha 1 is terminal default.
fn convert_color(c: syntect::highlighting::Color) -> Color {
    match c.a {
        0 => Color::Ansi(c.r),
        1 => Color::Reset,
        _ => Color::Rgb(c.r, c.g, c.b),
    }
}

fn selected_theme(theme: &Theme, light: bool) -> Arc<SynTheme> {
    static THEMES: std::sync::Mutex<Option<lru::LruCache<String, Arc<SynTheme>>>> = std::sync::Mutex::new(None);
    let Some(document) = &theme.syntax else {
        return default_syn_theme(light);
    };
    let mut cache = THEMES.lock().unwrap();
    let cache = cache.get_or_insert_with(|| lru::LruCache::new(std::num::NonZeroUsize::new(16).unwrap()));
    if let Some(theme) = cache.get(document) {
        return theme.clone();
    }
    let parsed = syntect::highlighting::ThemeSet::load_from_reader(&mut std::io::Cursor::new(document.as_bytes()))
        .map(Arc::new)
        .unwrap_or_else(|_| default_syn_theme(light));
    cache.put(document.clone(), parsed.clone());
    parsed
}

fn identity(theme: &Theme) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    mare_shared::style::theme_gen().hash(&mut h);
    theme.syntax.hash(&mut h);
    theme.styles.len().hash(&mut h);
    for key in [
        "md.code_block",
        "MareCodeBlock",
        "MareText",
        "MareMuted",
        "MareDiffAddText",
        "MareDiffDeleteText",
        "MareAccent",
    ] {
        theme.style(key).hash(&mut h);
    }
    h.finish()
}

/// Resolve the first explicitly styled scope, preserving UI defaults for unspecified fields.
pub(crate) fn scope_style(theme: &Theme, scopes: &[&str], fallback: Style) -> Style {
    if theme.syntax.is_none() {
        return fallback;
    }
    let selected = selected_theme(theme, is_light(theme));
    let highlighter = syntect::highlighting::Highlighter::new(&selected);
    let mut style = fallback;
    for name in scopes {
        let Ok(scope) = syntect::parsing::Scope::new(name) else {
            continue;
        };
        let modifier = highlighter.style_mod_for_stack(&[scope]);
        if modifier.foreground.is_none() && modifier.background.is_none() && modifier.font_style.is_none() {
            continue;
        }
        if let Some(fg) = modifier.foreground {
            style.fg = Some(convert_color(fg));
        }
        if let Some(bg) = modifier.background {
            style.bg = Some(convert_color(bg));
        }
        if let Some(font) = modifier.font_style {
            style.bold = font.contains(FontStyle::BOLD);
            style.italic = font.contains(FontStyle::ITALIC);
            style.underline = font.contains(FontStyle::UNDERLINE);
        }
        break;
    }
    style
}

fn diff_one_line(raw: &str, theme: &Theme) -> Line {
    let l = expand_tabs(raw.trim_end_matches(['\n', '\r']));
    let st = if l.starts_with("+++") || l.starts_with("---") {
        theme.fg("MareMuted").bold()
    } else if l.starts_with('+') {
        theme.fg("MareDiffAddText")
    } else if l.starts_with('-') {
        theme.fg("MareDiffDeleteText")
    } else if l.starts_with("@@") {
        theme.fg("MareAccent")
    } else {
        theme.fg("MareText")
    };
    let scopes: &[&str] = if l.starts_with("+++") || l.starts_with("---") {
        &["meta.diff.header"]
    } else if l.starts_with('+') {
        &["markup.inserted", "diff.inserted"]
    } else if l.starts_with('-') {
        &["markup.deleted", "diff.deleted"]
    } else if l.starts_with("@@") {
        &["meta.diff.range"]
    } else {
        &["source.diff"]
    };
    let mut st = scope_style(theme, scopes, st);
    if st.bg == Some(Color::Reset) {
        st.bg = None;
    }
    Line::styled(l, st)
}

/// Unified-diff fences: colored by line prefix (no grammar needed).
fn diff_lines(code: &str, theme: &Theme) -> Vec<Line> {
    code.lines().map(|l| diff_one_line(l, theme)).collect()
}

fn plain_lines(code: &str, theme: &Theme) -> Vec<Line> {
    let st = theme.fg("text");
    code.lines().map(|l| Line::styled(expand_tabs(l), st)).collect()
}

/// Highlight a code snippet into lines. Background colors from the syntect theme are dropped.
/// Highlighting is cached by (content hash, language) in a bounded LRU (256 blocks):
/// streaming re-renders the same code block
/// every frame, and syntect is the expensive part. A full cache evicts only its least recently
/// used entry — never everything at once.
static CACHE: std::sync::Mutex<Option<lru::LruCache<u64, std::sync::Arc<Vec<Line>>>>> = std::sync::Mutex::new(None);
static CACHE_CAP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(DEFAULT_CACHE_CAP);
/// Default highlight cache size (code blocks); `perf.highlight_cache` overrides it.
pub const DEFAULT_CACHE_CAP: usize = 256;

fn cache_cap() -> std::num::NonZeroUsize {
    std::num::NonZeroUsize::new(CACHE_CAP.load(std::sync::atomic::Ordering::Relaxed).max(1)).unwrap()
}

/// Set the LRU capacity (blocks); configurable from init.lua via `perf.highlight_cache`.
pub fn set_cache_capacity(n: usize) {
    CACHE_CAP.store(n.max(1), std::sync::atomic::Ordering::Relaxed);
    if let Some(c) = CACHE.lock().unwrap().as_mut() {
        c.resize(cache_cap());
    }
}

/// Entries currently cached (tests, `/profile`).
pub fn cache_len() -> usize {
    CACHE.lock().unwrap().as_ref().map_or(0, |c| c.len())
}

/// Skip syntect highlighting for inputs larger than 512 KiB.
pub(crate) const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
/// Skip syntect highlighting for inputs with more than 10,000 lines.
pub(crate) const MAX_HIGHLIGHT_LINES: usize = 10_000;
/// Skip syntect highlighting when any single line exceeds 4 KiB (minified JS/JSON/SVG).
pub(crate) const MAX_HIGHLIGHT_LINE_BYTES: usize = 4 * 1024;

pub(crate) fn exceeds_aggregate_highlight_limits(total_bytes: usize, total_lines: usize) -> bool {
    total_bytes > MAX_HIGHLIGHT_BYTES || total_lines > MAX_HIGHLIGHT_LINES
}

pub(crate) fn exceeds_highlight_limits(code: &str) -> bool {
    if code.len() > MAX_HIGHLIGHT_BYTES {
        return true;
    }
    let mut lines = 0usize;
    for raw in code.split_inclusive('\n') {
        lines += 1;
        if lines > MAX_HIGHLIGHT_LINES || raw.trim_end_matches(['\n', '\r']).len() > MAX_HIGHLIGHT_LINE_BYTES {
            return true;
        }
    }
    false
}

pub fn highlight(code: &str, lang: &str, theme: &Theme) -> Vec<Line> {
    use std::hash::{Hash, Hasher};
    if exceeds_highlight_limits(code) {
        return plain_lines(code, theme);
    }
    let light = is_light(theme);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    code.hash(&mut h);
    lang.hash(&mut h);
    identity(theme).hash(&mut h);
    let key = h.finish();
    if let Some(hit) = CACHE.lock().unwrap().as_mut().and_then(|c| c.get(&key).cloned()) {
        // callers restyle / wrap the lines, so they get their own copy of the shared result
        return hit.as_ref().clone();
    }
    let lines = highlight_uncached(code, lang, theme, light);
    let mut g = CACHE.lock().unwrap();
    g.get_or_insert_with(|| lru::LruCache::new(cache_cap())).put(key, std::sync::Arc::new(lines.clone()));
    lines
}

fn highlight_uncached(code: &str, lang: &str, theme: &Theme, light: bool) -> Vec<Line> {
    if exceeds_highlight_limits(code) {
        return plain_lines(code, theme);
    }
    if matches!(lang.trim().to_ascii_lowercase().as_str(), "diff" | "patch" | "udiff") {
        return diff_lines(code, theme);
    }
    // curated set first; rare languages fall back to syntect's full (compressed, embedded) set,
    // decompressed on first use; syntaxes with uncompilable patterns degrade to plain text
    let Some((ss, syntax)) = resolve_syntax(lang) else {
        return plain_lines(code, theme);
    };
    let selected = selected_theme(theme, light);
    let mut h = HighlightLines::new(syntax, &selected);
    code.split_inclusive('\n')
        .map(|raw| {
            highlight_one(&mut h, ss, raw)
                .unwrap_or_else(|_| Line::raw(expand_tabs(raw).trim_end_matches(['\n', '\r'])))
        })
        .collect()
}

fn highlight_one(h: &mut HighlightLines<'_>, ss: &SyntaxSet, raw: &str) -> Result<Line, syntect::Error> {
    let line_src = expand_tabs(raw);
    let regions = h.highlight_line(&line_src, ss)?;
    let mut line = Line::empty();
    for (st, text) in regions {
        let text = text.trim_end_matches(['\n', '\r']);
        if text.is_empty() {
            continue;
        }
        let mut s = Style::new().fg(convert_color(st.foreground));
        if st.font_style.contains(FontStyle::BOLD) {
            s = s.bold();
        }
        if st.font_style.contains(FontStyle::ITALIC) {
            s = s.italic();
        }
        line.push(Span::styled(text, s));
    }
    Ok(line)
}

/// Per-stream highlighter for an append-only, still-open code block.
#[derive(Default)]
pub(crate) struct StreamingHighlight {
    active: Option<HighlightCheckpoint>,
}

enum CheckpointKind {
    Syntect { set: &'static SyntaxSet, state: (syntect::highlighting::HighlightState, syntect::parsing::ParseState) },
    Plain(Style),
    Diff,
}

struct HighlightCheckpoint {
    lang: String,
    identity: u64,
    source: String,
    known_append_only: bool,
    completed: usize,
    kind: CheckpointKind,
    lines: Vec<Line>,
}

impl StreamingHighlight {
    /// Incrementally highlight `code` for an open fence, returning the cached completed lines slice
    /// and an optional incomplete trailing line. When `known_append` is true (caller guarantees
    /// `code` is an append-only `\n`-only extension), avoids storing/scanning `source`.
    pub(crate) fn update(
        &mut self,
        code: &str,
        lang: &str,
        theme: &Theme,
        known_append: bool,
    ) -> (&[Line], Option<Line>) {
        let id = identity(theme);
        let is_diff = matches!(lang.trim().to_ascii_lowercase().as_str(), "diff" | "patch" | "udiff");
        let fresh = self.active.as_ref().is_some_and(|c| {
            c.lang == lang
                && c.identity == id
                && if known_append {
                    c.known_append_only
                        && code.len() >= c.completed
                        && (c.completed == 0 || code.as_bytes()[c.completed - 1] == b'\n')
                } else {
                    !c.known_append_only && code.starts_with(&c.source)
                }
        });
        if !fresh {
            let kind = if is_diff {
                CheckpointKind::Diff
            } else if exceeds_highlight_limits(code) {
                CheckpointKind::Plain(theme.fg("text"))
            } else {
                match resolve_syntax(lang) {
                    Some((set, syntax)) => {
                        let selected = selected_theme(theme, is_light(theme));
                        let state = HighlightLines::new(syntax, &selected).state();
                        CheckpointKind::Syntect { set, state }
                    }
                    None => CheckpointKind::Plain(theme.fg("text")),
                }
            };
            self.active = Some(HighlightCheckpoint {
                lang: lang.to_owned(),
                identity: id,
                source: String::new(),
                known_append_only: known_append,
                completed: 0,
                kind,
                lines: Vec::new(),
            });
        }
        let c = self.active.as_mut().expect("initialized above");
        let end = code.rfind('\n').map_or(0, |n| n + 1);
        let light = is_light(theme);
        if matches!(c.kind, CheckpointKind::Syntect { .. }) {
            let new_completed_exceeds = code.len() > MAX_HIGHLIGHT_BYTES || {
                let mut total_lines = c.lines.len();
                code[c.completed..end].split_inclusive('\n').any(|raw| {
                    total_lines += 1;
                    total_lines > MAX_HIGHLIGHT_LINES
                        || raw.trim_end_matches(['\n', '\r']).len() > MAX_HIGHLIGHT_LINE_BYTES
                })
            };
            if new_completed_exceeds {
                let st = theme.fg("text");
                c.kind = CheckpointKind::Plain(st);
                c.lines = code[..end]
                    .split_inclusive('\n')
                    .map(|raw| Line::styled(expand_tabs(raw.trim_end_matches(['\n', '\r'])), st))
                    .collect();
                c.completed = end;
            }
        }
        match &mut c.kind {
            CheckpointKind::Syntect { set, state } => {
                let selected = selected_theme(theme, light);
                let mut failed = false;
                if c.completed < end {
                    let mut h = HighlightLines::from_state(&selected, state.0.clone(), state.1.clone());
                    let plain_st = theme.fg("text");
                    for raw in code[c.completed..end].split_inclusive('\n') {
                        if failed {
                            c.lines.push(Line::styled(expand_tabs(raw.trim_end_matches(['\n', '\r'])), plain_st));
                        } else if let Ok(line) = highlight_one(&mut h, set, raw) {
                            c.lines.push(line);
                        } else {
                            failed = true;
                            c.lines.push(Line::styled(expand_tabs(raw.trim_end_matches(['\n', '\r'])), plain_st));
                        }
                    }
                    if !failed {
                        *state = h.state();
                    }
                    c.completed = end;
                }
                if !known_append {
                    c.source.push_str(&code[c.source.len()..]);
                }
                let tail = (end < code.len()).then(|| {
                    let raw_tail = &code[end..];
                    let plain = || Line::styled(expand_tabs(raw_tail.trim_end_matches(['\n', '\r'])), theme.fg("text"));
                    if failed
                        || raw_tail.trim_end_matches(['\n', '\r']).len() > MAX_HIGHLIGHT_LINE_BYTES
                        || c.lines.len() >= MAX_HIGHLIGHT_LINES
                    {
                        plain()
                    } else {
                        let mut h = HighlightLines::from_state(&selected, state.0.clone(), state.1.clone());
                        match highlight_one(&mut h, set, raw_tail) {
                            Ok(line) => line,
                            Err(_) => {
                                failed = true;
                                plain()
                            }
                        }
                    }
                });
                if failed {
                    c.kind = CheckpointKind::Plain(theme.fg("text"));
                }
                (&c.lines, tail)
            }
            CheckpointKind::Plain(st) => {
                let st = *st;
                if c.completed < end {
                    for raw in code[c.completed..end].split_inclusive('\n') {
                        c.lines.push(Line::styled(expand_tabs(raw.trim_end_matches(['\n', '\r'])), st));
                    }
                    c.completed = end;
                }
                if !known_append {
                    c.source.push_str(&code[c.source.len()..]);
                }
                let tail = (end < code.len())
                    .then(|| Line::styled(expand_tabs(code[end..].trim_end_matches(['\n', '\r'])), st));
                (&c.lines, tail)
            }
            CheckpointKind::Diff => {
                if c.completed < end {
                    for raw in code[c.completed..end].split_inclusive('\n') {
                        c.lines.push(diff_one_line(raw, theme));
                    }
                    c.completed = end;
                }
                if !known_append {
                    c.source.push_str(&code[c.source.len()..]);
                }
                let tail = (end < code.len()).then(|| diff_one_line(&code[end..], theme));
                (&c.lines, tail)
            }
        }
    }

    pub(crate) fn highlight(&mut self, code: &str, lang: &str, theme: &Theme) -> Vec<Line> {
        if matches!(lang.trim().to_ascii_lowercase().as_str(), "diff" | "patch" | "udiff")
            || exceeds_highlight_limits(code)
            || resolve_syntax(lang).is_none()
        {
            self.active = None;
            return highlight(code, lang, theme);
        }
        let (completed, tail) = self.update(code, lang, theme, false);
        let mut lines = Vec::with_capacity(completed.len() + usize::from(tail.is_some()));
        lines.extend(completed.iter().cloned());
        if let Some(tl) = tail {
            lines.push(tl);
        }
        lines
    }
}

/// Highlight a single line using the syntax guessed from a file path (stateless; good enough for diffs).
pub fn highlight_line_for_path(line: &str, path: &str, theme: &Theme) -> Line {
    if line.trim_end_matches(['\n', '\r']).len() > MAX_HIGHLIGHT_LINE_BYTES {
        return Line::styled(expand_tabs(line.trim_end_matches(['\n', '\r'])), theme.fg("text"));
    }
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("");
    let name = std::path::Path::new(path).file_name().and_then(|e| e.to_str()).unwrap_or("");
    let lang = if ext.is_empty() { name } else { ext };
    highlight(line, lang, theme).into_iter().next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_background_theme_changes_invalidate_code_and_streaming_colors() {
        let mut a = Theme::default();
        let mut b = a.clone();
        a.set_style("MareDiffAddText", Style::new().fg(Color::Rgb(1, 2, 3)));
        b.set_style("MareDiffAddText", Style::new().fg(Color::Rgb(4, 5, 6)));
        assert_ne!(highlight("+added", "diff", &a), highlight("+added", "diff", &b));
        assert_ne!(identity(&a), identity(&b));
        assert_eq!(convert_color(syntect::highlighting::Color { r: 42, g: 0, b: 0, a: 0 }), Color::Ansi(42));
        assert_eq!(convert_color(syntect::highlighting::Color { r: 0, g: 0, b: 0, a: 1 }), Color::Reset);
        let document = |fg: &str| {
            format!(
                r#"<?xml version="1.0"?><plist version="1.0"><dict><key>settings</key><array><dict><key>settings</key><dict><key>foreground</key><string>{fg}</string></dict></dict><dict><key>scope</key><string>keyword, storage, markup.inserted</string><key>settings</key><dict><key>foreground</key><string>{fg}</string></dict></dict></array></dict></plist>"#
            )
        };
        a.syntax = Some(document("#112233"));
        b.syntax = Some(document("#aabbcc"));
        let code = "fn main() {}\n";
        assert_ne!(highlight(code, "rust", &a), highlight(code, "rust", &b));
        assert_eq!(highlight("+a", "diff", &a)[0].spans[0].style.fg, Some(Color::Rgb(0x11, 0x22, 0x33)));
        let background_doc = document("#112233").replace("markup.inserted", "diff.inserted").replace(
            "</string></dict></dict></array>",
            "</string><key>background</key><string>#445566</string></dict></dict></array>",
        );
        let mut with_background = a.clone();
        with_background.syntax = Some(background_doc);
        let diff = highlight("+a", "diff", &with_background);
        assert_eq!(diff[0].spans[0].style.bg, Some(Color::Rgb(0x44, 0x55, 0x66)));
        let edit = crate::diff::render_diff("", "added", None, 80, &with_background);
        assert!(edit.iter().any(|line| line.fill_bg == Some(Color::Rgb(0x44, 0x55, 0x66))));
        let mut stream = StreamingHighlight::default();
        let first = stream.highlight(code, "rust", &a);
        let second = stream.highlight(code, "rust", &b);
        assert_ne!(first, second, "same source must be replayed after a theme switch");
        assert_eq!(stream.highlight(code, "rust", &a), first, "cancel restores syntax colors");
    }

    /// Regenerate assets/ from the curated grammar sources in assets/syntaxes (sublimehq/Packages
    /// at the commit syntect's defaults use) + one theme.
    #[test]
    #[ignore = "asset generator, not a test: regenerates assets/ when run with --ignored"]
    fn gen_syntax_assets() {
        use syntect::highlighting::ThemeSet;
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/syntaxes");
        let mut b = syntect::parsing::SyntaxSetBuilder::new();
        b.add_plain_text_syntax();
        let mut files: Vec<_> = std::fs::read_dir(&src).unwrap().flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            match syntect::parsing::SyntaxDefinition::load_from_str(&text, true, None) {
                Ok(def) => b.add(def),
                Err(e) => eprintln!("skip {}: {e}", f.display()),
            }
        }
        let ss = b.build();
        for s in ss.syntaxes() {
            eprintln!("kept: {}", s.name);
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        syntect::dumps::dump_to_uncompressed_file(&ss, dir.join("syntaxes.packdump")).unwrap();
        let mut ts = ThemeSet::load_defaults();
        let th = ts.themes.remove("base16-ocean.dark").unwrap();
        syntect::dumps::dump_to_file(&th, dir.join("theme.themedump")).unwrap();
    }

    #[test]
    #[ignore = "asset generator, not a test: regenerates the light theme asset when run with --ignored"]
    fn gen_light_theme_asset() {
        let mut ts = syntect::highlighting::ThemeSet::load_defaults();
        let th = ts.themes.remove("InspiredGitHub").unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        syntect::dumps::dump_to_file(&th, dir.join("theme-light.themedump")).unwrap();
    }

    /// the light flavor highlights with light-theme (dark) colors.
    #[test]
    fn light_flavor_uses_dark_syntax_colors() {
        let mut light = Theme::default();
        light.set_style("md.code_block", Style::new().bg(Color::Rgb(0xe5, 0xe7, 0xeb)));
        assert!(is_light(&light) && !is_light(&Theme::default()));
        let lum = |lines: &[Line]| {
            let (mut sum, mut n) = (0.0, 0.0);
            for sp in lines.iter().flat_map(|l| &l.spans) {
                if let Some(Color::Rgb(r, g, b)) = sp.style.fg {
                    sum += 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
                    n += 1.0;
                }
            }
            sum / n
        };
        let code = "fn main() {\n    let s = \"hi\"; // note\n}\n";
        let dark = lum(&highlight(code, "rust", &Theme::default()));
        let on_light = lum(&highlight(code, "rust", &light));
        assert!(on_light < 110.0 && dark > on_light + 40.0, "dark {dark} light {on_light}");
    }

    #[test]
    fn known_and_unknown() {
        let t = Theme::default();
        let l = highlight("fn main() {\n\tlet x = 1;\n}\n", "rust", &t);
        assert_eq!(l.len(), 3);
        assert!(l[0].spans.len() > 1);
        assert_eq!(l[1].plain(), "    let x = 1;");
        let p = highlight("a\nb", "nonexistent-lang", &t);
        assert_eq!(p.len(), 2);
        assert_ne!(
            highlight("+a\n-b", "diff", &t)[0].spans[0].style,
            highlight("+a\n-b", "diff", &t)[1].spans[0].style
        );
        for lang in ["ts", "tsx", "lua", "zsh", "toml", "yaml", "go", "cpp", "json", "py"] {
            assert!(find_syntax(lang).is_some(), "{lang}");
        }
    }

    #[test]
    fn ansi_and_base16_256_tmthemes_preserve_ansi_indices_and_reset() {
        let t = Theme {
            syntax: Some(
                r##"<?xml version="1.0"?>
<plist version="1.0"><dict><key>settings</key><array>
<dict><key>settings</key><dict><key>foreground</key><string>#00000001</string><key>background</key><string>#00000001</string></dict></dict>
<dict><key>scope</key><string>keyword, storage</string><key>settings</key><dict><key>foreground</key><string>#05000000</string></dict></dict>
<dict><key>scope</key><string>constant.numeric</string><key>settings</key><dict><key>foreground</key><string>#10000000</string></dict></dict>
<dict><key>scope</key><string>string</string><key>settings</key><dict><key>foreground</key><string>#02000000</string></dict></dict>
</array></dict></plist>"##
                    .into(),
            ),
            ..Default::default()
        };
        let lines = highlight("fn main() { let x = 42; \"ok\"; }\n", "rust", &t);
        let fgs: Vec<Option<Color>> = lines[0].spans.iter().map(|s| s.style.fg).collect();
        assert!(fgs.contains(&Some(Color::Ansi(5))), "expected keyword Ansi(5): {fgs:?}");
        assert!(fgs.contains(&Some(Color::Ansi(16))), "expected numeric Ansi(16): {fgs:?}");
        assert!(fgs.contains(&Some(Color::Ansi(2))), "expected string Ansi(2): {fgs:?}");
        assert!(
            lines[0].spans.iter().all(|s| s.style.bg.is_none()),
            "terminal-default bg (#00000001) should stay unset: {:?}",
            lines[0].spans
        );
    }

    #[test]
    fn default_syntax_themes_share_arc_across_lookups() {
        let dark = Theme::default();
        let mut light = Theme::default();
        light.set_style("md.code_block", Style::new().bg(Color::Rgb(240, 240, 240)));
        let d1 = selected_theme(&dark, false);
        let d2 = selected_theme(&dark, false);
        let l1 = selected_theme(&light, true);
        let l2 = selected_theme(&light, true);
        assert!(Arc::ptr_eq(&d1, &d2), "default dark SynTheme must be shared");
        assert!(Arc::ptr_eq(&l1, &l2), "default light SynTheme must be shared");
        assert!(!Arc::ptr_eq(&d1, &l1));
    }

    #[test]
    fn oversize_inputs_bypass_syntect_and_do_not_pollute_cache() {
        let t = Theme::default();
        let long_line = format!("let x = \"{}\";", "a".repeat(MAX_HIGHLIGHT_LINE_BYTES + 10));
        let lines = highlight(&long_line, "rust", &t);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].style, t.fg("text"));

        // Oversize input must not evict an existing cached entry from the LRU cache.
        let small = "fn keep_me() -> usize { 1 }\n";
        let cached = highlight(small, "rust", &t);
        assert!(cached[0].spans.len() > 1);
        let big = "fn big() {}\n".repeat(MAX_HIGHLIGHT_LINES + 5);
        let big_lines = highlight(&big, "rust", &t);
        assert_eq!(big_lines.len(), MAX_HIGHLIGHT_LINES + 5);
        assert_eq!(big_lines[0].spans[0].style, t.fg("text"));

        // StreamingHighlight downgrades cleanly when exceeding line length cap.
        let mut sh = StreamingHighlight::default();
        let (done, tail) = sh.update(&long_line, "rust", &t, false);
        assert!(done.is_empty());
        assert_eq!(tail.unwrap().spans[0].style, t.fg("text"));
    }

    #[test]
    fn all_selectable_syntaxes_compile_every_pattern() {
        for (label, ss) in [("curated", syntax_set()), ("full", full_syntax_set())] {
            let defs = ss.clone().into_builder();
            let mut failures = Vec::new();
            for (idx, syn) in ss.syntaxes().iter().enumerate() {
                if !syntax_patterns_valid(ss, defs.syntaxes(), idx) {
                    failures.push(format!("{label}: {} ({})", syn.name, syn.scope));
                }
            }
            assert!(failures.is_empty(), "syntaxes with uncompilable patterns: {failures:#?}");
        }
    }

    #[test]
    fn markdown_fenced_code_block_highlights_on_full_and_streaming_paths() {
        let t = Theme::default();
        // Minimal fixtures covering backtick fences, extended closing fences, tilde fences,
        // and block-quoted fences (all hit Markdown's `fenced_code_block_end` backref rule).
        let doc = concat!(
            "# Title\n",
            "```rust\n",
            "fn main() {\n",
            "    let x = 1;\n",
            "}\n",
            "````\n",
            "~~~\n",
            "plain\n",
            "~~~~\n",
            "> ```\n",
            "> quoted\n",
            "> ```\n",
        );
        let expected: Vec<&str> = doc.lines().collect();
        let lines = highlight(doc, "md", &t);
        let actual: Vec<String> = lines.iter().map(Line::plain).collect();
        assert_eq!(actual, expected, "rendered markdown text must be complete");
        assert!(lines.iter().any(|l| l.spans.len() > 1), "markdown must still be syntax-highlighted");

        // Streaming path: complete lines + unfinished tail line, followed by a normal code block.
        let mut sh = StreamingHighlight::default();
        let partial = "```rust\nfn main() {\n    let x";
        let (done1, tail1) = sh.update(partial, "md", &t, false);
        assert_eq!(done1.iter().map(Line::plain).collect::<Vec<_>>(), vec!["```rust", "fn main() {"]);
        assert_eq!(tail1.as_ref().map(Line::plain).as_deref(), Some("    let x"));

        let complete = format!("{partial} = 1;\n}}\n```\n");
        let (done2, tail2) = sh.update(&complete, "md", &t, false);
        assert!(tail2.is_none());
        assert_eq!(
            done2.iter().map(Line::plain).collect::<Vec<_>>(),
            vec!["```rust", "fn main() {", "    let x = 1;", "}", "```"]
        );

        let next = sh.highlight("fn next() -> u32 { 42 }\n", "rust", &t);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].plain(), "fn next() -> u32 { 42 }");
        assert!(next[0].spans.len() > 1, "subsequent normal code block must still be highlighted");
    }

    #[test]
    fn broken_syntax_degrades_to_plain_text_without_panicking_or_retrying() {
        // Mirrors the real Markdown fenced-code-block bug: passes YAML load because `has_captures`
        // skips syntect's load-time check, then panics with TargetNotRepeatable when `\3` is unset.
        let yaml = r#"%YAML 1.2
---
name: BrokenRegexTest
file_extensions: [brokenregextest]
scope: source.brokenregextest
contexts:
  main:
    - match: '((`)``+|(~)~~+)'
      push: body
  body:
    - match: '\1(?:(?:\2)*|(?:\3)*)'
      pop: true
"#;
        let mut b = syntect::parsing::SyntaxSetBuilder::new();
        b.add_plain_text_syntax();
        b.add(SyntaxDefinition::load_from_str(yaml, true, None).unwrap());
        let leaked: &'static SyntaxSet = Box::leak(Box::new(b.build()));

        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                TEST_SYNTAX_SET.with(|slot| *slot.borrow_mut() = None);
            }
        }
        TEST_SYNTAX_SET.with(|slot| *slot.borrow_mut() = Some(leaked));
        let _guard = Guard;

        let t = Theme::default();
        let broken_key = leaked.find_syntax_by_token("brokenregextest").unwrap() as *const SyntaxReference as usize;
        let code = "```\nline one\n```\n";
        let lines = highlight(code, "brokenregextest", &t);
        assert_eq!(lines.iter().map(Line::plain).collect::<Vec<_>>(), vec!["```", "line one", "```"]);
        assert!(lines.iter().all(|l| l.spans.len() == 1 && l.spans[0].style == t.fg("text")));
        assert_eq!(
            VALIDATED_SYNTAXES.lock().unwrap().as_ref().and_then(|m| m.get(&broken_key)),
            Some(&false),
            "failed syntax must be marked plain-text-only for the process"
        );

        // Incremental streaming path: stays Plain across frames without retrying syntect.
        let mut sh = StreamingHighlight::default();
        let (d1, tail1) = sh.update("```\nsec", "brokenregextest", &t, false);
        assert_eq!(d1.iter().map(Line::plain).collect::<Vec<_>>(), vec!["```"]);
        assert_eq!(tail1.as_ref().map(Line::plain).as_deref(), Some("sec"));
        assert!(matches!(sh.active.as_ref().unwrap().kind, CheckpointKind::Plain(_)));

        let (d2, tail2) = sh.update("```\nsecond\n```", "brokenregextest", &t, false);
        assert_eq!(d2.iter().map(Line::plain).collect::<Vec<_>>(), vec!["```", "second"]);
        assert_eq!(tail2.as_ref().map(Line::plain).as_deref(), Some("```"));
        assert!(matches!(sh.active.as_ref().unwrap().kind, CheckpointKind::Plain(_)));

        // Next normal block is still syntax-highlighted.
        let next = sh.highlight("let ok = 1;\n", "rust", &t);
        assert_eq!(next[0].plain(), "let ok = 1;");
        assert!(next[0].spans.len() > 1, "normal syntax must stay highlighted after broken syntax fallback");
    }
}
