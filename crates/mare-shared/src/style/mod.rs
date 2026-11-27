//! Styled text primitives shared by every renderer component.
//!
//! A component renders to `Vec<Line>`; a `Line` is a sequence of styled `Span`s.
//! All width math is done in terminal display cells (CJK / emoji aware).
//!
//! `model` holds the types (`Color`, `Style`, `Span`, `Line`), `width` the cell measure,
//! `cells` truncation and slicing, `wrap` word wrapping, `theme` the named styles and `sgr` the
//! ANSI encoding. The layout generation stays here: width and theme writes both bump it.

use std::collections::HashMap;
use std::fmt::Write as _;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

mod cells;
mod model;
mod sgr;
mod theme;
mod width;
mod wrap;

pub use cells::{display_path, expand_tabs, overlay_line, sanitize, slice_cells, truncate_line, truncate_str};
pub use model::{Color, Line, Span, Style, link_id, theme_name, theme_name_id};
pub use sgr::{ColorMode, line_to_ansi, no_color_env, resolve_icons, style_sgr};
pub use theme::{MdConfig, SPINNER_FRAMES, SPINNER_STILL, Theme};
pub use width::{
    WidthMethod, ambiwidth_double, grapheme_width, set_ambiwidth_double, set_width_method, set_width_overrides,
    str_width, width_method,
};
pub use wrap::{is_url_like, wrap_line, wrap_line_indent};

/// Layout generation: bumped whenever something that changes rendered output without changing
/// the content does — a theme / style / Markdown option write, width rules, `ambiwidth`. Render
/// caches keyed by content include it, so a theme switch never shows stale colors.
static THEME_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn theme_gen() -> u64 {
    THEME_GEN.load(std::sync::atomic::Ordering::Acquire)
}

/// Call after writing the shared theme (or anything else that changes how content renders).
pub fn theme_changed() {
    THEME_GEN.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

#[cfg(test)]
mod tests;
