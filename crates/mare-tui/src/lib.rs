//! Main-screen TUI: finished content goes to native scrollback, a live area at the bottom
//! (streaming message, widgets, editor, status bar) is diff-rendered with synchronized output.

pub mod diff;
pub mod editor;
pub mod fallback;
pub mod frame;
pub mod highlight;
pub mod image;
pub mod image_header;
pub mod image_hold;
pub mod image_queue;
pub mod keys;
pub mod markdown;
pub mod modes;
pub mod out;
pub mod render;
pub mod schedule;
pub use mare_shared::style;
pub mod term;

/// Functions the binding layer needs from the TUI (render nodes, normalize key names),
/// injected through `UiShared::hooks` so lower crates don't depend on the TUI.
pub fn hooks() -> mare_shared::ui::UiHooks {
    mare_shared::ui::UiHooks {
        render: |node, width, theme| render::render_node(node, width, theme, 0),
        normalize_key: keys::normalize_key,
        osc52: out::osc52,
    }
}
pub mod capture;
pub mod keyconflict;
pub mod kitty_marker;
#[cfg(unix)]
pub(crate) mod live_input;
pub mod paste;
pub mod queue_keys;
pub mod reflow;
pub mod search;
pub mod snapshot;
pub mod stream;
pub mod term_input;
pub mod termcaps;
pub mod termdetect;
pub mod tty;
pub mod winprobe;
