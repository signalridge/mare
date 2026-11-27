//! Synchronous bridge between `App`'s live `Editor` and `mare_shared::ui::UiShared`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use mare_shared::ui::{EditorBridge, EditorStateSnapshot, EditorSyncOp};
use mare_tui::editor::Editor;

type CursorPos = (usize, usize);
type SelectionSpan = Option<(CursorPos, CursorPos)>;

#[derive(Default)]
struct SharedEditorTracker {
    revision: u64,
    cached_text_rev: u64,
    has_cached_text: bool,
    cached_text: Arc<String>,
    published_text_rev: u64,
    published_text: Arc<String>,
    last_synced_text: Arc<String>,
    cursor: (usize, usize),
    selection: Option<((usize, usize), (usize, usize))>,
    has_last: bool,
    changed_from_lua: bool,
}

pub(in crate::app) struct SyncedEditorState {
    pub(in crate::app) text: Arc<String>,
    pub(in crate::app) cursor: (usize, usize),
    pub(in crate::app) selection: Option<((usize, usize), (usize, usize))>,
    pub(in crate::app) anchor: Option<(usize, usize)>,
    pub(in crate::app) head: Option<(usize, usize)>,
    pub(in crate::app) selected_text: Option<String>,
    pub(in crate::app) revision: u64,
    pub(in crate::app) shell_level: u8,
    pub(in crate::app) live_elements: Option<HashSet<u64>>,
    pub(in crate::app) lua_text_changed: bool,
}

#[derive(Clone)]
pub(in crate::app) struct SharedEditor {
    editor: Arc<Mutex<Editor>>,
    tracker: Arc<Mutex<SharedEditorTracker>>,
    external_editor_active: Arc<AtomicBool>,
}

impl Default for SharedEditor {
    fn default() -> Self {
        Self::new(Editor::new())
    }
}

impl SharedEditor {
    pub(in crate::app) fn new(editor: Editor) -> Self {
        Self {
            editor: Arc::new(Mutex::new(editor)),
            tracker: Arc::new(Mutex::new(SharedEditorTracker::default())),
            external_editor_active: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(in crate::app) fn set_external_editor_active(&self, active: bool) {
        self.external_editor_active.store(active, Ordering::Release);
    }

    pub(in crate::app) fn lock(&self) -> MutexGuard<'_, Editor> {
        self.editor.lock().unwrap()
    }

    pub(in crate::app) fn replace_editor(&self, editor: Editor) {
        *self.editor.lock().unwrap() = editor;
        let mut tracker = self.tracker.lock().unwrap();
        tracker.has_last = false;
        tracker.has_cached_text = false;
        tracker.cached_text_rev = 0;
        tracker.published_text_rev = 0;
    }

    fn materialize_cached_text<'a>(tracker: &'a mut SharedEditorTracker, editor: &Editor) -> &'a Arc<String> {
        let text_rev = editor.text_revision();
        if !tracker.has_cached_text || tracker.cached_text_rev != text_rev {
            let raw = editor.raw_text();
            if !tracker.has_cached_text || *tracker.cached_text != raw {
                tracker.cached_text = Arc::new(raw);
            }
            tracker.cached_text_rev = text_rev;
            tracker.has_cached_text = true;
        }
        &tracker.cached_text
    }

    pub(in crate::app) fn cached_raw_text(&self) -> Arc<String> {
        let editor = self.editor.lock().unwrap();
        let mut tracker = self.tracker.lock().unwrap();
        Arc::clone(Self::materialize_cached_text(&mut tracker, &editor))
    }

    fn refresh_tracker(
        tracker: &mut SharedEditorTracker,
        editor: &Editor,
        mark_lua_change: bool,
    ) -> (CursorPos, SelectionSpan, SelectionSpan, bool) {
        let shell_shift = editor.shell_level() as usize;
        let shift_out = |(r, c): (usize, usize)| (r, if r == 0 { c + shell_shift } else { c });
        let cursor = shift_out(editor.cursor());
        let selection = editor.selection().map(|(a, b)| (shift_out(a), shift_out(b)));
        let endpoints = editor.selection_endpoints().map(|(a, b)| (shift_out(a), shift_out(b)));
        let text_rev = editor.text_revision();
        let text_changed = if tracker.has_last && tracker.published_text_rev == text_rev {
            false
        } else {
            let text = Arc::clone(Self::materialize_cached_text(tracker, editor));
            let changed =
                !tracker.has_last || (!Arc::ptr_eq(&text, &tracker.published_text) && *text != *tracker.published_text);
            if changed {
                tracker.published_text = text;
            }
            tracker.published_text_rev = text_rev;
            changed
        };
        if !tracker.has_last || text_changed || tracker.cursor != cursor || tracker.selection != selection {
            tracker.revision += 1;
            tracker.cursor = cursor;
            tracker.selection = selection;
            tracker.has_last = true;
        }
        if mark_lua_change {
            tracker.changed_from_lua = true;
        }
        (cursor, selection, endpoints, text_changed)
    }

    fn build_snapshot(&self, editor: &Editor, mark_lua_change: bool) -> EditorStateSnapshot {
        let mut tracker = self.tracker.lock().unwrap();
        let (cursor, selection, endpoints, _) = Self::refresh_tracker(&mut tracker, editor, mark_lua_change);
        EditorStateSnapshot {
            text: tracker.published_text.as_ref().clone(),
            cursor,
            selection,
            anchor: endpoints.map(|x| x.0),
            head: endpoints.map(|x| x.1),
            selected_text: editor.selected_text(),
            empty: editor.is_empty(),
            revision: tracker.revision,
        }
    }

    pub(in crate::app) fn sync_state(
        &self,
        ui_snap: &RwLock<EditorStateSnapshot>,
        last_revision: u64,
    ) -> Option<SyncedEditorState> {
        let editor = self.editor.lock().unwrap();
        let shell_level = editor.shell_level();
        let mut tracker = self.tracker.lock().unwrap();
        let (cursor, selection, endpoints, _) = Self::refresh_tracker(&mut tracker, &editor, false);
        let revision = tracker.revision;
        let changed_from_lua = std::mem::take(&mut tracker.changed_from_lua);
        let same_text_as_synced = Arc::ptr_eq(&tracker.published_text, &tracker.last_synced_text);
        let ui_matches = {
            let cur = ui_snap.read().unwrap();
            cur.revision == revision
                && cur.cursor == cursor
                && cur.selection == selection
                && (same_text_as_synced || cur.text == *tracker.published_text)
        };
        if ui_matches && revision == last_revision && !changed_from_lua {
            return None;
        }
        let anchor = endpoints.map(|x| x.0);
        let head = endpoints.map(|x| x.1);
        let selected_text = editor.selected_text();
        let empty = editor.is_empty();
        if !ui_matches {
            let mut cur = ui_snap.write().unwrap();
            if cur.revision == last_revision && last_revision != 0 && same_text_as_synced {
                cur.cursor = cursor;
                cur.selection = selection;
                cur.anchor = anchor;
                cur.head = head;
                cur.selected_text = selected_text.clone();
                cur.empty = empty;
                cur.revision = revision;
            } else {
                *cur = EditorStateSnapshot {
                    text: tracker.published_text.as_ref().clone(),
                    cursor,
                    selection,
                    anchor,
                    head,
                    selected_text: selected_text.clone(),
                    empty,
                    revision,
                };
            }
        }
        let lua_text_changed =
            changed_from_lua && !same_text_as_synced && *tracker.published_text != *tracker.last_synced_text;
        tracker.last_synced_text = Arc::clone(&tracker.published_text);
        let live_elements = changed_from_lua.then(|| editor.referenced_element_ids());
        Some(SyncedEditorState {
            text: Arc::clone(&tracker.published_text),
            cursor,
            selection,
            anchor,
            head,
            selected_text,
            revision,
            shell_level,
            live_elements,
            lua_text_changed,
        })
    }
}

impl EditorBridge for SharedEditor {
    fn snapshot(&self) -> EditorStateSnapshot {
        let editor = self.editor.lock().unwrap();
        self.build_snapshot(&editor, false)
    }

    fn apply(&self, op: EditorSyncOp) -> Result<EditorStateSnapshot, String> {
        if self.external_editor_active.load(Ordering::Acquire) {
            return Err("editor is busy while external editor is open".into());
        }
        let mut editor = self.editor.lock().unwrap();
        let rev_before = editor.text_revision();
        let check_same_set_text = matches!(&op, EditorSyncOp::SetText(_));
        let text_before = check_same_set_text.then(|| editor.raw_text());
        let shell_shift = editor.shell_level() as usize;
        let shift_in = |(r, c): (usize, usize)| (r, if r == 0 { c.saturating_sub(shell_shift) } else { c });
        match op {
            EditorSyncOp::SetText(t) => editor.set_text(&t),
            EditorSyncOp::SetCursor { row, col } => {
                let (r, c) = shift_in((row, col));
                editor.set_cursor(r, c);
            }
            EditorSyncOp::SetSelection { anchor, head } => {
                editor.set_selection(anchor.map(shift_in), head.map(shift_in));
            }
            EditorSyncOp::Insert(t) => editor.insert_at_cursor(&t),
            EditorSyncOp::Replace { start, end, text } => {
                editor.replace_range(start.map(shift_in), end.map(shift_in), &text);
            }
        }
        let text_changed = if let Some(before) = text_before {
            editor.text_revision() != rev_before && editor.raw_text() != before
        } else {
            editor.text_revision() != rev_before
        };
        Ok(self.build_snapshot(&editor, text_changed))
    }

    fn take_changed(&self) -> bool {
        let mut tracker = self.tracker.lock().unwrap();
        std::mem::take(&mut tracker.changed_from_lua)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_only_moves_reuse_cached_text_without_text_revision_bump() {
        let mut ed = Editor::new();
        let big: String = (0..2000).map(|i| format!("line-{i}")).collect::<Vec<_>>().join("\n");
        ed.set_text(&big);
        let shared = SharedEditor::new(ed);
        let ui_snap = RwLock::new(EditorStateSnapshot::default());
        let mut last_rev = 0;

        let s1 = shared.sync_state(&ui_snap, last_rev).expect("initial sync");
        last_rev = s1.revision;
        let last_text = s1.text;
        let text_rev_before = shared.lock().text_revision();

        // No-op sync returns None.
        assert!(shared.sync_state(&ui_snap, last_rev).is_none());

        // Cursor-only move does not bump text_revision and shares the exact same Arc<String>.
        shared.lock().set_cursor(500, 3);
        assert_eq!(shared.lock().text_revision(), text_rev_before);
        let s2 = shared.sync_state(&ui_snap, last_rev).expect("cursor sync");
        assert!(s2.revision > last_rev);
        assert!(Arc::ptr_eq(&s2.text, &last_text), "must reuse cached Arc<String>");
        assert_eq!(s2.cursor, (500, 3));
        assert!(s2.live_elements.is_none());

        // Lua SetCursor does not mark text changed.
        let s3 = shared.apply(EditorSyncOp::SetCursor { row: 100, col: 2 }).unwrap();
        assert_eq!(s3.cursor, (100, 2));
        assert!(!shared.take_changed());
    }

    #[test]
    fn completion_read_and_equal_length_replace_do_not_swallow_sync_changes() {
        let mut ed = Editor::new();
        ed.set_text("@ab");
        ed.set_cursor(0, 1);
        let shared = SharedEditor::new(ed);
        let ui_snap = RwLock::new(EditorStateSnapshot::default());

        let s1 = shared.sync_state(&ui_snap, 0).expect("initial sync");
        assert_eq!(s1.text.as_str(), "@ab");
        assert_eq!(s1.cursor, (0, 1));
        let mut rev = s1.revision;

        // Forward-delete keeps cursor at (0, 1); completion reads cached_raw_text() BEFORE sync_state.
        shared.lock().apply(mare_tui::editor::TextOp::DeleteForward);
        assert_eq!(shared.cached_raw_text().as_str(), "@b");
        let s2 = shared.sync_state(&ui_snap, rev).expect("delete after cached_raw_text must still sync");
        assert!(s2.revision > rev, "revision must advance after forward-delete");
        assert_eq!(s2.text.as_str(), "@b");
        assert_eq!(ui_snap.read().unwrap().text, "@b");
        rev = s2.revision;

        // Equal-length replacement with same cursor must still update ui_snap and emit sync.
        shared.lock().replace_range(Some((0, 0)), Some((0, 2)), "@z");
        shared.lock().set_cursor(0, 1);
        assert_eq!(shared.cached_raw_text().as_str(), "@z");
        let s3 = shared.sync_state(&ui_snap, rev).expect("equal-length replacement must sync");
        assert!(s3.revision > rev);
        assert_eq!(s3.text.as_str(), "@z");
        assert_eq!(ui_snap.read().unwrap().text, "@z");
    }

    #[test]
    fn sync_apply_rejects_mutations_while_external_editor_is_active() {
        let mut ed = Editor::new();
        ed.set_text("draft");
        let shared = SharedEditor::new(ed);

        shared.set_external_editor_active(true);
        let err = shared.apply(EditorSyncOp::SetText("overwrite".into())).unwrap_err();
        assert!(err.contains("external editor"), "unexpected error: {err}");
        assert_eq!(shared.snapshot().text, "draft", "rejected apply must not mutate editor");

        shared.set_external_editor_active(false);
        let snap = shared.apply(EditorSyncOp::SetText("updated".into())).unwrap();
        assert_eq!(snap.text, "updated");
    }
}
