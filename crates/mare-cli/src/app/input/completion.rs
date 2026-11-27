//! Completion: concurrent multi-source scheduling (`mare.ui.completion.register`) and popup accept.

use mare_binding::ui::completion::{collect_ranked, source_matches};

use crate::app::*;

#[derive(Clone, PartialEq, Eq)]
pub(in crate::app) struct CompletionItem {
    /// Owning source id, when the item came from a registered completion source.
    pub(in crate::app) source_id: Option<u64>,
    /// icon and named style from the completion source
    pub(in crate::app) icon: Option<String>,
    pub(in crate::app) style: Option<String>,
    pub(in crate::app) label: String,
    pub(in crate::app) insert: String,
    pub(in crate::app) detail: Option<String>,
    /// grey argument hint after the label (`/model [name]`)
    pub(in crate::app) hint: Option<String>,
    /// Enter on a bare command runs it right away (command metadata `auto_execute`)
    pub(in crate::app) auto_execute: bool,
}

pub(in crate::app) struct Completion {
    pub(in crate::app) items: Vec<CompletionItem>,
    pub(in crate::app) selected: usize,
}

/// The completion popup and the sequence number of the latest request: a result for an older
/// request (the text changed, or was sent, since it was asked for) is dropped.
#[derive(Default)]
pub(in crate::app) struct CompletionState {
    pub(in crate::app) popup: Option<Completion>,
    pub(in crate::app) hint: Option<(u64, String)>,
    popup_scope: Option<(usize, usize, bool)>,
    popup_token: Option<String>,
    pending_scope: Option<(usize, usize, bool)>,
    pending_token: Option<String>,
    seq: u64,
    shell_level: u8,
}

impl CompletionState {
    pub(in crate::app) fn set_shell_level(&mut self, shell_level: u8) {
        if self.shell_level != shell_level {
            self.shell_level = shell_level;
            self.invalidate();
        }
    }

    /// The sequence number of a new request (results of earlier ones no longer apply).
    fn next_seq(&mut self, scope: (usize, usize, bool), token: String) -> u64 {
        self.seq += 1;
        self.pending_scope = Some(scope);
        self.pending_token = Some(token);
        self.seq
    }

    /// Close the popup and drop the results of requests still on their way.
    pub(in crate::app) fn invalidate(&mut self) {
        self.popup = None;
        self.hint = None;
        self.popup_scope = None;
        self.popup_token = None;
        self.pending_scope = None;
        self.pending_token = None;
        self.seq += 1;
    }

    /// The results of request `seq`: they open the popup (no items close it) unless a later
    /// request or [`Self::invalidate`] superseded them.
    pub(in crate::app) fn apply(&mut self, seq: u64, items: Vec<CompletionItem>, hint: Option<(u64, String)>) {
        if seq == self.seq {
            self.popup = if items.is_empty() { None } else { Some(Completion { items, selected: 0 }) };
            self.popup_scope = self.popup.as_ref().and(self.pending_scope);
            self.popup_token = if self.popup.is_some() { self.pending_token.clone() } else { None };
            self.hint = hint;
        }
    }

    pub(in crate::app) fn is_open(&self) -> bool {
        self.popup.is_some()
    }
}

struct CompletionContext {
    start: usize,
    token: String,
    row: usize,
    shell_level: u8,
    command: Option<String>,
    empty_argument: bool,
}

impl CompletionContext {
    fn scope(&self) -> (usize, usize, bool) {
        (self.row, self.start, self.command.is_some())
    }
}

impl App {
    fn completion_context(&self) -> Option<CompletionContext> {
        let ed = self.ed();
        let shell_level = ed.shell_level();
        let (row, col) = ed.cursor();
        let line = ed.current_line_str();
        let prefix = if row == 0 && shell_level == 0 { line.get(..col).unwrap_or("") } else { "" };
        let command = prefix.split_once(char::is_whitespace).and_then(|(name, _)| {
            (name.starts_with('/') && name.len() > 1 && !name[1..].contains('/')).then(|| name.to_string())
        });
        let empty_argument = command.as_ref().is_some_and(|name| prefix[name.len()..].trim().is_empty());
        let (start, token) = ed.current_token().or_else(|| command.as_ref().map(|_| (col, String::new())))?;
        // in shell mode the line starts after the `!` marks: a `/path` there is no command
        let start = if row == 0 { start + shell_level as usize } else { start };
        Some(CompletionContext { start, token, row, shell_level, command, empty_argument })
    }

    /// True only when the open completion popup was computed for the editor's current token and
    /// scope. Prevents fast typing before an async completion reply from accepting a stale candidate.
    pub(super) fn completion_matches_current_token(&self) -> bool {
        let (Some(popup_scope), Some(popup_token)) =
            (self.completion.popup_scope, self.completion.popup_token.as_deref())
        else {
            return false;
        };
        self.completion_context().is_some_and(|ctx| ctx.scope() == popup_scope && ctx.token == popup_token)
    }

    pub(in crate::app) fn request_completion(&mut self, force: bool) {
        let Some(ctx) = self.completion_context() else {
            self.completion.invalidate();
            return;
        };
        let scope = ctx.scope();
        let CompletionContext { start, token, row, shell_level, command, empty_argument } = ctx;
        if shell_level > 0 && !force {
            self.completion.invalidate();
        }
        // Entering a new token/context (e.g. moving from `/cmd` into `/cmd args`) makes any
        // open popup from the previous token stale so Enter executes instead of accepting `/cmd`.
        if self.completion.popup_scope.is_some_and(|prev| prev != scope) {
            self.completion.invalidate();
        }
        let has_match = self.ui.completion_sources.read().unwrap().iter().any(|src| source_matches(src, &token, force));
        if !has_match && command.is_none() {
            self.completion.invalidate();
            return;
        }
        let raw_text = self.editor.cached_raw_text();
        let seq = self.completion.next_seq(scope, token.clone());
        let h = self.host.clone();
        let ui = self.ui.clone();
        let tx = self.app_tx.clone();
        let payload = serde_json::json!({
            "token": token,
            "start": start,
            "line_index": row,
            "text": raw_text.as_str(),
            "force": force,
            "shell_level": shell_level,
            "shell_mode": shell_level > 0,
        });
        tokio::task::spawn_local(async move {
            let lua = h.lua();
            let ev = mare_shared::lua::json_to_lua(lua, &payload).unwrap_or(mlua::Value::Nil);
            let ranked = collect_ranked(lua, &ui, &ev, &token, force).await;
            let mut items = Vec::with_capacity(ranked.len());
            if command.is_some()
                && let Some(serde_json::Value::Array(arguments)) = h.bail_json("input/complete", payload.clone()).await
            {
                for item in arguments {
                    let s = |k: &str| item.get(k).and_then(serde_json::Value::as_str).map(str::to_string);
                    let Some(label) = s("label") else { continue };
                    items.push(CompletionItem {
                        source_id: None,
                        icon: s("icon"),
                        style: s("style"),
                        insert: s("insert").unwrap_or_else(|| label.clone()),
                        label,
                        detail: s("detail"),
                        hint: s("hint"),
                        auto_execute: false,
                    });
                }
            }
            for (source_id, t) in ranked {
                let s = |k: &str| t.get::<Option<String>>(k).ok().flatten();
                let label = s("label").unwrap_or_default();
                let insert = s("insert").unwrap_or_else(|| label.clone());
                let auto_execute = t.get::<Option<bool>>("auto_execute").ok().flatten().unwrap_or(false);
                items.push(CompletionItem {
                    source_id: Some(source_id),
                    icon: s("icon"),
                    style: s("style"),
                    label,
                    insert,
                    detail: s("detail"),
                    hint: s("hint"),
                    auto_execute,
                });
            }
            let hint = if empty_argument {
                let name = command.as_deref().unwrap_or_default();
                let mut hint_payload = payload.clone();
                hint_payload["token"] = serde_json::json!(name);
                hint_payload["start"] = serde_json::json!(0);
                let hint_ev = mare_shared::lua::json_to_lua(lua, &hint_payload).unwrap_or(mlua::Value::Nil);
                collect_ranked(lua, &ui, &hint_ev, name, false).await.into_iter().find_map(|(id, item)| {
                    let label = item.get::<Option<String>>("label").ok().flatten()?;
                    (label == name)
                        .then(|| item.get::<Option<String>>("hint").ok().flatten().map(|hint| (id, hint)))
                        .flatten()
                })
            } else {
                None
            };
            let _ = tx.send(AppMsg::Completions(seq, items, hint));
        });
    }

    pub(in crate::app) fn apply_completions(
        &mut self,
        seq: u64,
        mut items: Vec<CompletionItem>,
        hint: Option<(u64, String)>,
    ) {
        let active = self.ui.completion_sources.read().unwrap();
        items.retain(|it| it.source_id.is_none_or(|id| active.iter().any(|s| s.id == id)));
        let hint = hint.filter(|(id, _)| active.iter().any(|s| s.id == *id));
        drop(active);
        self.completion.apply(seq, items, hint);
    }

    pub(super) fn accept_completion(&mut self, c: Completion) {
        if let Some(it) = c.items.get(c.selected) {
            self.ed().replace_current_token(&it.insert);
        }
    }
}
