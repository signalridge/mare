//! Core keymap contexts and action registry.
//!
//! Kept free of `KeyCode::` and UI key-name literals.

/// A core action known to the keymap: name, `/hotkeys` description, supported contexts, and whether
/// leaving it with no key must be flagged in `/hotkeys` and `mare doctor`.
pub struct CoreAction {
    pub name: &'static str,
    pub desc: &'static str,
    pub contexts: &'static [&'static str],
    pub essential: bool,
    pub declines: bool,
}

const fn act(name: &'static str, desc: &'static str, contexts: &'static [&'static str]) -> CoreAction {
    CoreAction { name, desc, contexts, essential: false, declines: false }
}

const fn declining_act(name: &'static str, desc: &'static str, contexts: &'static [&'static str]) -> CoreAction {
    CoreAction { name, desc, contexts, essential: false, declines: true }
}

const fn essential_act(name: &'static str, desc: &'static str, contexts: &'static [&'static str]) -> CoreAction {
    CoreAction { name, desc, contexts, essential: true, declines: false }
}

/// Every core action handled by `App::run_action_on` / `App::view_action`.
/// Descriptions intentionally name no key so rebinds stay accurate.
pub const CORE_ACTIONS: &[CoreAction] = &[
    act("interrupt", "interrupt the agent · twice when idle: rewind picker (/undo)", &["editor"]),
    act("transcript", "expanded transcript", &["editor"]),
    act("keymap-help", "all key bindings (on an empty input)", &["editor"]),
    act("toggle-expand", "toggle compact / expanded tool output and thinking", &["editor"]),
    declining_act("quit-if-empty", "exit when the input is empty", &["editor"]),
    act("quit", "quit", &["editor"]),
    CoreAction {
        name: "suspend",
        #[cfg(windows)]
        desc: "suspend is not supported on Windows",
        #[cfg(not(windows))]
        desc: "suspend mare (resume with fg)",
        contexts: &["editor"],
        essential: false,
        declines: false,
    },
    act("select-mode", "select text in the live area with the mouse, release copies", &["editor"]),
    act("redraw", "redraw the screen", &["global"]),
    act("steer", "send the input into the running turn", &["editor"]),
    act("history-search", "search input history", &["editor"]),
    act("follow-up", "queue the input as a follow-up", &["editor"]),
    act("complete", "complete @file or /command", &["editor"]),
    act("newline", "insert a newline", &["editor"]),
    act("external-editor", "edit the input in $EDITOR", &["editor"]),
    act("paste-image", "paste an image from the clipboard", &["editor"]),
    essential_act("editor.submit", "send the input", &["editor"]),
    act("text.newline", "new line", &["text"]),
    act("text.left", "one character left", &["text"]),
    act("text.right", "one character right", &["text"]),
    act("text.word_left", "one word left", &["text"]),
    act("text.word_right", "one word right", &["text"]),
    act("text.line_start", "start of line", &["text"]),
    act("text.line_end", "end of line", &["text"]),
    act("text.up", "line up", &["text"]),
    act("text.down", "line down", &["text"]),
    act("text.select_left", "extend selection one character left", &["text"]),
    act("text.select_right", "extend selection one character right", &["text"]),
    act("text.select_word_left", "extend selection one word left", &["text"]),
    act("text.select_word_right", "extend selection one word right", &["text"]),
    act("text.select_line_start", "extend selection to start of line", &["text"]),
    act("text.select_line_end", "extend selection to end of line", &["text"]),
    act("text.select_up", "extend selection one line up", &["text"]),
    act("text.select_down", "extend selection one line down", &["text"]),
    act("text.select_all", "select all text", &["text"]),
    act("text.delete_back", "delete the character before the cursor", &["text"]),
    act("text.delete_word_back", "delete the word before the cursor", &["text"]),
    act("text.delete_forward", "delete the character after the cursor", &["text"]),
    act("text.delete_word_forward", "delete the word after the cursor", &["text"]),
    act("text.kill_line_start", "delete to the start of the line", &["text"]),
    act("text.kill_line_end", "delete to the end of the line", &["text"]),
    act("text.yank", "paste the last deletion", &["text"]),
    act("text.yank_pop", "cycle older deletions", &["text"]),
    act("text.undo", "undo the last edit", &["text"]),
    act("text.redo", "redo the last undone edit", &["text"]),
    essential_act("input.accept", "confirm input", &["input"]),
    essential_act("input.cancel", "cancel input", &["input"]),
    act("view.close", "close the transcript", &["transcript"]),
    act("view.back", "clear the search, else close the transcript", &["transcript"]),
    act("view.up", "scroll up a line", &["transcript"]),
    act("view.down", "scroll down a line", &["transcript"]),
    act("view.page_up", "scroll up a page", &["transcript"]),
    act("view.page_down", "scroll down a page", &["transcript"]),
    act("view.top", "go to the top", &["transcript"]),
    act("view.bottom", "go to the bottom", &["transcript"]),
    act("view.search", "search", &["transcript"]),
    act("view.copy", "copy the message at the reading position", &["transcript"]),
    act("view.next_match", "next search match", &["transcript"]),
    act("view.prev_match", "previous search match", &["transcript"]),
    act("select.up", "previous item", &["select"]),
    act("select.down", "next item", &["select"]),
    act("select.page_up", "page up", &["select"]),
    act("select.page_down", "page down", &["select"]),
    act("select.top", "first item", &["select"]),
    act("select.bottom", "last item", &["select"]),
    act("select.accept", "choose the selected item", &["select"]),
    act("select.back", "clear search / close", &["select"]),
    act("select.search", "search (fuzzy)", &["select"]),
    act("select.tab_next", "next tab", &["select"]),
    act("select.tab_prev", "previous tab", &["select"]),
    act("select.help", "all keys", &["select"]),
    act("select.toggle", "toggle selection", &["select"]),
    act("select.toggle_preview", "toggle preview", &["select"]),
    act("select.preview_up", "scroll preview up", &["select"]),
    act("select.preview_down", "scroll preview down", &["select"]),
    act("select.preview_page_up", "scroll preview page up", &["select"]),
    act("select.preview_page_down", "scroll preview page down", &["select"]),
    act("select.preview_top", "scroll preview to top", &["select"]),
    act("select.preview_bottom", "scroll preview to bottom", &["select"]),
    act("select.body_up", "scroll body up", &["select"]),
    act("select.body_down", "scroll body down", &["select"]),
    act("select.body_page_up", "scroll body page up", &["select"]),
    act("select.body_page_down", "scroll body page down", &["select"]),
    act("select.body_top", "scroll body to top", &["select"]),
    act("select.body_bottom", "scroll body to bottom", &["select"]),
    act("confirm.yes", "answer yes", &["confirm"]),
    act("confirm.no", "answer no", &["confirm"]),
    act("confirm.toggle", "switch choice", &["confirm"]),
    act("confirm.accept", "confirm choice", &["confirm"]),
    act("confirm.cancel", "cancel", &["confirm"]),
    act("confirm.scroll_up", "scroll up", &["confirm"]),
    act("confirm.scroll_down", "scroll down", &["confirm"]),
    act("confirm.page_up", "scroll page up", &["confirm"]),
    act("confirm.page_down", "scroll page down", &["confirm"]),
    act("confirm.top", "scroll to top", &["confirm"]),
    act("confirm.bottom", "scroll to bottom", &["confirm"]),
    act("help.up", "previous item", &["help"]),
    act("help.down", "next item", &["help"]),
    act("help.accept", "run selected key", &["help"]),
    act("help.cancel", "close", &["help"]),
    act("info.up", "scroll up", &["info"]),
    act("info.down", "scroll down", &["info"]),
    act("info.page_up", "scroll page up", &["info"]),
    act("info.page_down", "scroll page down", &["info"]),
    act("info.top", "scroll to top", &["info"]),
    act("info.bottom", "scroll to bottom", &["info"]),
    act("info.close", "close overlay", &["info"]),
];

/// A registered keymap context (core or plugin-declared).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextDef {
    pub parents: Vec<String>,
    pub core: bool,
    pub bindable: bool,
    /// When true, unbound unmodified printable keys stop at the `before_global` editor chain
    /// instead of falling through to text insertion or `global`.
    pub swallow_printable: bool,
}

/// Core contexts installed in every `Keymap`: `(name, parents, bindable)`.
pub const CORE_CONTEXTS: &[(&str, &[&str], bool)] = &[
    ("global", &[], true),
    ("text", &[], true),
    ("editor", &["text", "global"], true),
    ("editor.completion", &["editor"], true),
    ("editor.queue", &["editor"], true),
    ("editor.history", &["editor"], true),
    ("editor.selection", &["editor"], true),
    ("transcript", &["global"], true),
    ("transcript.search", &["transcript"], true),
    ("select", &[], true),
    ("select.vim", &["select"], true),
    ("select.search", &["select"], true),
    ("input", &["text"], true),
    ("confirm", &[], true),
    ("help", &[], true),
    ("info", &[], true),
];

/// Look up a core action by name.
pub fn find_core_action(name: &str) -> Option<&'static CoreAction> {
    CORE_ACTIONS.iter().find(|a| a.name == name)
}

/// Whether `seg` matches `^[a-z][a-z0-9_-]*$`.
pub fn is_valid_context_segment(seg: &str) -> bool {
    let bytes = seg.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_lowercase()
        && bytes[1..].iter().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_action_constructors_set_expected_flags_and_registry_invariants() {
        let plain = act("demo", "desc", &["editor"]);
        assert!(!plain.essential && !plain.declines && plain.contexts == ["editor"]);

        let dec = declining_act("demo.dec", "desc", &["editor"]);
        assert!(!dec.essential && dec.declines);

        let ess = essential_act("demo.ess", "desc", &["input"]);
        assert!(ess.essential && !ess.declines);

        // Every core action must be findable and reference only declared core contexts.
        for action in CORE_ACTIONS {
            let found = find_core_action(action.name).expect("core action must be indexed");
            assert_eq!(found.name, action.name);
            assert!(!action.contexts.is_empty(), "core action {} has no contexts", action.name);
            for ctx in action.contexts {
                assert!(
                    CORE_CONTEXTS.iter().any(|(name, _, _)| name == ctx),
                    "core action {} references unknown context {ctx}",
                    action.name
                );
            }
        }
        assert!(find_core_action("nonexistent.action").is_none());
        assert!(is_valid_context_segment("editor_1-a"));
        assert!(!is_valid_context_segment(""));
        assert!(!is_valid_context_segment("1bad"));
        assert!(!is_valid_context_segment("Bad"));
    }
}
