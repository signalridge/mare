//! Which key does what while messages wait in the agent's queue:
//! with an empty input ↑ selects a queued message, ↑/↓ move between them, Enter loads the
//! selected one into the input to edit it in place, Delete removes it and Esc leaves; while one
//! is being edited Esc puts it back unchanged (it never interrupts the turn: like a completion
//! menu, a queue edit consumes Esc first) and Enter on an emptied input removes it.
//! The state lives in the `ui` plugin (`plugins/lua/ui/queue_edit.lua`); this maps a resolved
//! action to its `input/queue-*` event before the action reaches history or the editor.

/// `mare.ui.set_queue_mode`: nothing selected, a message selected, a message being edited.
pub const OFF: u8 = 0;
pub const SELECTING: u8 = 1;
pub const EDITING: u8 = 2;

/// What the key does.
#[derive(Debug, PartialEq)]
pub enum Route {
    /// Not a queue key: the normal key handling goes on.
    Pass,
    /// Send this event to the `ui` plugin and swallow the key; the mode the app should assume
    /// until the plugin answers.
    Emit { event: &'static str, delta: Option<i32>, mode: u8 },
    /// A selection is open and some other key came: leave it, then handle the key normally.
    LeaveThenPass,
}

fn emit(event: &'static str, mode: u8) -> Route {
    Route::Emit { event, delta: None, mode }
}

/// `queued`: messages waiting; `editor_empty`: nothing typed; `browsing_history`: ↑ is walking
/// the input history; `completion_open`: a completion menu owns Esc, ↑ and Enter.
pub fn route(
    mode: u8,
    queued: usize,
    editor_empty: bool,
    browsing_history: bool,
    completion_open: bool,
    action: Option<&str>,
) -> Route {
    if queued == 0 || completion_open {
        return Route::Pass;
    }
    match (mode, action) {
        (SELECTING, Some("text.up")) => Route::Emit { event: "input/queue-move", delta: Some(-1), mode },
        (SELECTING, Some("text.down")) => Route::Emit { event: "input/queue-move", delta: Some(1), mode },
        (SELECTING, Some("editor.submit")) => emit("input/queue-edit", EDITING),
        (SELECTING, Some("text.delete_forward" | "text.delete_back")) => emit("input/queue-remove", SELECTING),
        (SELECTING, Some("interrupt")) => emit("input/queue-cancel", OFF),
        (SELECTING, None) => Route::LeaveThenPass,
        (SELECTING, Some(_)) => Route::Pass,
        (EDITING, Some("interrupt")) => emit("input/queue-cancel", OFF),
        (EDITING, Some("editor.submit")) if editor_empty => emit("input/queue-remove", OFF),
        (EDITING, _) => Route::Pass,
        (_, Some("text.up")) if editor_empty && !browsing_history => emit("input/queue-select", SELECTING),
        (_, Some("editor.submit")) if editor_empty && !browsing_history => emit("input/queue-menu", OFF),
        _ => Route::Pass,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(mode: u8, empty: bool, action: &str) -> Route {
        route(mode, 2, empty, false, false, Some(action))
    }

    #[test]
    fn up_on_an_empty_input_selects_and_enter_opens_the_menu() {
        assert_eq!(r(OFF, true, "text.up"), emit("input/queue-select", SELECTING));
        assert_eq!(r(OFF, true, "editor.submit"), emit("input/queue-menu", OFF));
        assert_eq!(r(OFF, false, "text.up"), Route::Pass);
        assert_eq!(route(OFF, 0, true, false, false, Some("text.up")), Route::Pass);
        assert_eq!(route(OFF, 2, true, true, false, Some("text.up")), Route::Pass);
    }

    #[test]
    fn a_selection_takes_the_arrows_enter_delete_and_esc() {
        assert_eq!(
            r(SELECTING, true, "text.up"),
            Route::Emit { event: "input/queue-move", delta: Some(-1), mode: SELECTING }
        );
        assert_eq!(
            r(SELECTING, true, "text.down"),
            Route::Emit { event: "input/queue-move", delta: Some(1), mode: SELECTING }
        );
        assert_eq!(r(SELECTING, true, "editor.submit"), emit("input/queue-edit", EDITING));
        assert_eq!(r(SELECTING, true, "text.delete_forward"), emit("input/queue-remove", SELECTING));
        assert_eq!(r(SELECTING, true, "interrupt"), emit("input/queue-cancel", OFF));
        assert_eq!(r(SELECTING, true, "text.insert"), Route::Pass);
        assert_eq!(route(SELECTING, 2, true, false, false, None), Route::LeaveThenPass);
    }

    #[test]
    fn editing_only_takes_esc_and_enter_on_an_emptied_input() {
        assert_eq!(r(EDITING, false, "interrupt"), emit("input/queue-cancel", OFF));
        assert_eq!(r(EDITING, false, "editor.submit"), Route::Pass);
        assert_eq!(r(EDITING, true, "editor.submit"), emit("input/queue-remove", OFF));
        assert_eq!(r(EDITING, false, "text.up"), Route::Pass);
    }

    #[test]
    fn a_completion_menu_keeps_its_keys_and_a_chord_leaves_the_selection() {
        assert_eq!(route(EDITING, 2, false, false, true, Some("interrupt")), Route::Pass);
        assert_eq!(route(SELECTING, 2, true, false, false, None), Route::LeaveThenPass);
        assert_eq!(route(OFF, 2, true, false, false, None), Route::Pass);
    }
}
