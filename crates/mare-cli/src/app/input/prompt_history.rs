//! Prompt history: the history file, Ctrl+R search, a cleared draft kept.

use crate::app::*;

fn history_path() -> std::path::PathBuf {
    mare_shared::paths::state_dir().join("history.jsonl")
}

fn load_history() -> Vec<String> {
    std::fs::read_to_string(history_path())
        .map(|s| s.lines().filter_map(|l| serde_json::from_str::<String>(l).ok()).collect())
        .unwrap_or_default()
}

fn append_history(s: &str) {
    append_history_at(&history_path(), s);
}

fn append_history_at(path: &std::path::Path, s: &str) {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
        && mare_shared::private::create_dir_all(dir).is_err()
    {
        return;
    }
    if let Ok(mut f) = mare_shared::private::open(std::fs::OpenOptions::new().create(true).append(true), path) {
        let _ = writeln!(f, "{}", serde_json::to_string(s).unwrap_or_default());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn prompt_history_creates_a_private_file_in_an_existing_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("history.jsonl");
        append_history_at(&path, "a private prompt");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "\"a private prompt\"\n");
    }

    #[test]
    fn prompt_history_creates_a_private_parent() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        let path = dir.join("history.jsonl");
        append_history_at(&path, "a private prompt");
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

/// Where ↓ goes while browsing the history.
pub(super) enum HistNav {
    /// a newer entry
    Recall(String),
    /// past the newest entry: back to the draft typed before ↑
    Draft(String),
    /// not browsing: nothing to do
    Stay,
}

/// The prompt history and ↑/↓ browsing through it (written only here).
pub(in crate::app) struct PromptHistory {
    entries: Vec<String>,
    /// the entry shown while browsing (↑)
    cursor: Option<usize>,
    /// the input as it was before browsing started, put back by ↓ past the newest entry
    draft: String,
}

impl PromptHistory {
    /// The history file's entries; not browsing.
    pub(in crate::app) fn load() -> Self {
        Self { entries: load_history(), cursor: None, draft: String::new() }
    }

    /// `text` as the newest entry (and in the history file), unless it already is.
    pub(super) fn record(&mut self, text: &str) {
        if self.entries.last().map(String::as_str) != Some(text) {
            self.entries.push(text.to_string());
            append_history(text);
        }
    }

    /// Stop browsing.
    pub(in crate::app) fn reset(&mut self) {
        self.cursor = None;
    }

    /// ↑ was pressed and an entry is shown.
    pub(in crate::app) fn browsing(&self) -> bool {
        self.cursor.is_some()
    }

    /// ↑: the next older entry (the newest when browsing starts, keeping `current_raw()` as the
    /// draft); None with no history.
    pub(super) fn prev(&mut self, current_raw: impl FnOnce() -> String) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let idx = match self.cursor {
            None => {
                self.draft = current_raw();
                self.entries.len() - 1
            }
            Some(i) => i.saturating_sub(1),
        };
        self.cursor = Some(idx);
        Some(self.entries[idx].clone())
    }

    /// ↓: the next newer entry, or the draft past the newest one.
    pub(super) fn next(&mut self) -> HistNav {
        let Some(i) = self.cursor else { return HistNav::Stay };
        if i + 1 < self.entries.len() {
            self.cursor = Some(i + 1);
            HistNav::Recall(self.entries[i + 1].clone())
        } else {
            self.cursor = None;
            HistNav::Draft(std::mem::take(&mut self.draft))
        }
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Ctrl+R's list: newest first, each entry once, at most 500.
    fn search_entries(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.entries.iter().rev().filter(|h| seen.insert((*h).clone())).take(500).cloned().collect()
    }
}

impl App {
    /// Ctrl+C on a draft:
    /// the draft goes into the history first and the editor's undo (Ctrl+_) brings it back.
    pub(super) fn clear_draft(&mut self) {
        let text = self.ed().text();
        // a bare `!` / `!!` (shell mode, nothing typed) is no entry worth keeping
        if !matches!(text.trim(), "" | "!" | "!!") {
            self.history.record(&text);
        }
        self.history.reset();
        self.ed().clear_undoable();
        self.sync_editor_state();
    }

    /// Ctrl+R: reverse history search. Reuses the filterable select popup:
    /// type to filter, Enter puts the entry in the editor.
    pub(super) fn history_search(&mut self) {
        if self.history.is_empty() {
            self.push_notice(("No history yet".into(), "info".into(), Instant::now()));
            return;
        }
        let entries: Vec<String> = self.history.search_entries();
        let items =
            entries.iter().map(|h| SelectItem { label: h.replace('\n', " ⏎ "), ..Default::default() }).collect();
        let (tx, rx) = oneshot::channel();
        if self.select.active() {
            self.leave_select_mode();
        }
        self.reset_select_row_failures();
        self.modal = Some(Modal::Select {
            title: "Search history (type to filter)".into(),
            title_right: vec![],
            split: Default::default(),
            items,
            filter: String::new(),
            selected: 0,
            multi: false,
            accept_label: None,
            allow_search: true,
            tabs: vec![],
            tab: 0,
            kind: "list".into(),
            keys: vec![],
            body: vec![],
            body_scroll: 0,
            body_scroll_max: None,
            preview_scroll: 0,
            preview_scroll_max: None,
            preview_hidden: false,
            searching: true,
            vim: false,
            count: 0,
            highlight: None,
            last_hl: None,
            reply: Some(tx),
            token: 0,
            modal_id: 0,
            list_rev: 0,
            filter_cache: SelectFilterCache::default(),
            approval: false,
            drawn: false,
        });
        let app_tx = self.app_tx.clone();
        tokio::task::spawn_local(async move {
            if let Ok(Ok(r)) = rx.await
                && let Some(text) = entries.get(r.index)
            {
                let _ = app_tx.send(AppMsg::SetEditor(text.clone()));
            }
        });
    }
}
