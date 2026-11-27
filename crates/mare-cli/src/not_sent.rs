//! A prompt that is not sent: the transcript says "not sent: <reason>" in
//! the notice style of the `ui` plugin (`mare.ui.Notice`, error level) and the text goes back
//! into the input box, in front of anything typed since (restoring a hook-blocked prompt).
//! The Lua side does the same for refusals that come from the agent (`turn/completed {not_sent}`); this is the frontend's own case,
//! Enter while startup is in progress or blocked.

use mare_shared::style::{Line, Span, Theme};
use mare_shared::ui::Node;
use mare_tui::editor::Editor;

/// "● not sent: <reason>", wrapped under its text.
pub(crate) fn notice(theme: &Theme, reason: &str) -> Node {
    let style = theme.fg("MareError");
    Node::Paragraph {
        line: Line::new(vec![Span::styled("● ", style), Span::styled(format!("not sent: {reason}"), style)]),
        indent: vec![Span::raw("  ")],
        hard_wrap: false,
    }
}

/// Put `text` back into the input box: as it is when the box is empty, else in front of the
/// draft with a blank line between them.
pub(crate) fn restore(editor: &mut Editor, text: &str) {
    editor.prepend_text(text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_goes_in_front_of_a_draft() {
        let mut e = Editor::new();
        restore(&mut e, "first");
        assert_eq!(e.raw_text(), "first");
        e.set_text("typed since");
        restore(&mut e, "refused");
        assert_eq!(e.raw_text(), "refused\n\ntyped since");
    }
}
