//! Key routing inside an open modal.

use crate::app::view::modal::HelpAct;
use crate::app::*;
use mare_tui::keys::{fold_fullwidth_ascii, fold_fullwidth_key};

fn is_scroll_op(op: &str) -> bool {
    matches!(op, "up" | "scroll_up" | "down" | "scroll_down" | "page_up" | "page_down" | "top" | "bottom")
}

/// Context lookup chain for a list modal (`select.search` / `select.vim` / `select`).
pub(in crate::app) fn select_chain(searching: bool, vim: bool) -> &'static [&'static str] {
    if searching {
        &["select.search", "select"]
    } else if vim {
        &["select.vim", "select"]
    } else {
        &["select"]
    }
}

fn scroll_pane(scroll: &mut usize, max: Option<usize>, delta: isize) {
    let cur = max.map_or(*scroll, |m| (*scroll).min(m));
    if delta < 0 {
        *scroll = cur.saturating_sub(delta.unsigned_abs());
    } else if let Some(m) = max {
        *scroll = cur.saturating_add(delta as usize).min(m);
    } else {
        *scroll = cur.saturating_add(delta as usize);
    }
}

fn apply_scroll_op(op: &str, scroll: &mut usize, max: Option<usize>, page: isize) -> bool {
    match op {
        "up" | "scroll_up" => scroll_pane(scroll, max, -1),
        "down" | "scroll_down" => scroll_pane(scroll, max, 1),
        "page_up" => scroll_pane(scroll, max, -page),
        "page_down" => scroll_pane(scroll, max, page),
        "top" => *scroll = 0,
        "bottom" => *scroll = max.unwrap_or(usize::MAX),
        _ => return false,
    }
    true
}

impl App {
    pub(super) fn handle_modal_key(&mut self, key: KeyEvent) {
        self.handle_modal_key_at(key, app_now());
    }

    pub(super) fn handle_modal_key_at(&mut self, key: KeyEvent, at: Instant) {
        // Keystrokes inside a non-approval modal count as active user input so a queued approval
        // prompt still waits the typing idle cooldown instead of firing immediately under a held
        // or double-tapped confirmation key.
        if !self.is_approval_modal() {
            self.record_editor_activity(at);
        }
        self.handle_modal_key_inner(key);
        self.notify_highlight();
    }

    fn handle_modal_key_inner(&mut self, key: KeyEvent) {
        if matches!(self.modal, Some(Modal::Input { .. })) {
            self.handle_input_modal_key(key);
            return;
        }
        match &mut self.modal {
            None => {}
            Some(Modal::Select { kind, .. }) if kind == "confirm" => self.handle_confirm_key(key),
            Some(Modal::Select { .. }) => self.handle_select_key(key),
            Some(Modal::Help { .. }) => self.handle_help_key(key),
            Some(Modal::Input { .. }) => unreachable!(),
            Some(Modal::Info { .. }) => self.handle_info_key(key),
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) {
        let ks = key_to_string(&fold_fullwidth_key(key));
        let _ = self.dispatch_bindings_stops(&["confirm"], &ks, false);
    }

    fn handle_help_key(&mut self, key: KeyEvent) {
        if let Some(c) = mare_tui::keys::printable_char(&key) {
            if let Some(Modal::Help { filter, selected, .. }) = &mut self.modal {
                filter.push(c);
                *selected = 0;
            }
            return;
        }
        let ks = key_to_string(&key);
        if self.dispatch_bindings_stops(&["help"], &ks, false) {
            return;
        }
        if key.code == KeyCode::Backspace
            && let Some(Modal::Help { items, filter, selected, .. }) = &mut self.modal
        {
            filter.pop();
            *selected = skip_unselectable(items, &filter_items(items, filter), 0, true);
        }
    }

    fn handle_info_key(&mut self, key: KeyEvent) {
        let ks = key_to_string(&fold_fullwidth_key(key));
        let ctx = match &self.modal {
            Some(Modal::Info { context: Some(ctx), .. }) => Some(ctx.clone()),
            _ => None,
        };
        let custom = ctx.is_some();
        let owned = self.info_context_chain(ctx.as_deref());
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        let handled = self.dispatch_bindings_stops(&refs, &ks, false);
        if handled {
            return;
        }
        if custom {
            // Focused custom overlays resolve along `opts.context -> info` without falling through
            // to `global` and stay open on unbound non-Esc keys; Esc closes them.
            if ks == "esc" {
                self.pop_modal_or_promote_delayed();
            }
        } else if ks != "ctrl+d" {
            // Plain info overlays close on any unbound key other than Ctrl+D.
            self.pop_modal_or_promote_delayed();
        }
    }

    fn handle_select_key(&mut self, key: KeyEvent) {
        let ks = key_to_string(&key);
        let ks_folded = key_to_string(&fold_fullwidth_key(key));
        let (
            (prev_selected, prev_tab),
            (vim, searching, multi, allow_search, approval, drawn),
            selected_is_selectable,
            extra_hit,
        ) = {
            let Some(Modal::Select {
                items,
                filter,
                selected,
                tab,
                searching,
                keys,
                vim,
                multi,
                allow_search,
                filter_cache,
                approval,
                drawn,
                ..
            }) = &self.modal
            else {
                return;
            };
            let visible = filter_cache.get(items, *tab, filter);
            let sel_ok = visible.get(*selected).is_some_and(|&i| items[i].is_selectable());
            let ctrl_char = match key.code {
                KeyCode::Char(c)
                    if key.modifiers.contains(Modifiers::CONTROL)
                        && !key
                            .modifiers
                            .intersects(Modifiers::ALT | Modifiers::SUPER | Modifiers::HYPER | Modifiers::META)
                        && c.is_ascii_alphabetic() =>
                {
                    Some(((c.to_ascii_lowercase() as u8) & 0x1f) as char)
                }
                _ => None,
            };
            let extra = ctrl_char.filter(|cc| keys.iter().any(|(k, _)| k == cc)).or_else(|| {
                if ks == "tab" && keys.iter().any(|(k, _)| *k == '\t') {
                    return Some('\t');
                }
                match key.code {
                    KeyCode::Char(c)
                        if !*searching
                            && !key.modifiers.intersects(
                                Modifiers::CONTROL
                                    | Modifiers::ALT
                                    | Modifiers::SUPER
                                    | Modifiers::HYPER
                                    | Modifiers::META,
                            ) =>
                    {
                        let fc = fold_fullwidth_ascii(c);
                        keys.iter().any(|(k, _)| *k == fc).then_some(fc)
                    }
                    _ => None,
                }
            });
            ((*selected, *tab), (*vim, *searching, *multi, *allow_search, *approval, *drawn), sel_ok, extra)
        };

        // Never accept decision keys for an approval modal before it has been drawn at least once.
        if approval && !drawn {
            return;
        }

        if matches!(self.pending_prefix, Some((_, KeyFocus::Select))) {
            let chain = select_chain(searching, vim);
            let eff_ks = if searching { &ks } else { &ks_folded };
            if self.complete_prefix(KeyFocus::Select, &key, eff_ks, chain) {
                self.finish_select_key(prev_selected, prev_tab, false);
                return;
            }
        }

        // The multi-select toggle takes precedence over a same-key extra action.
        let chain = select_chain(searching, vim);
        if multi && !searching && extra_hit.is_some() && self.key_selects_action(chain, &ks_folded, "select.toggle") {
            self.run_bindings(chain, &ks_folded, false);
            self.finish_select_key(prev_selected, prev_tab, false);
            return;
        }

        // Plugin extra `keys` (`mare.ui.select{keys=...}`, ctrl keys and Tab included) win before the chain.
        if let Some(ch) = extra_hit {
            if selected_is_selectable {
                if let Some(Modal::Select { items, filter, selected, reply, tab, multi, filter_cache, .. }) =
                    &mut self.modal
                {
                    let vis = filter_cache.get(items, *tab, filter);
                    send_select_reply(reply, items, &vis, (*selected, *tab), filter, Some(ch), *multi);
                }
                self.pop_modal_or_promote_delayed();
            } else {
                self.finish_select_key(prev_selected, prev_tab, false);
            }
            return;
        }

        if searching {
            if self.try_prefix(&["select.search"], &ks, KeyFocus::Select)
                || self.run_bindings(&["select.search"], &ks, false)
            {
                self.finish_select_key(prev_selected, prev_tab, false);
                return;
            }
            if let Some(c) = mare_tui::keys::printable_char(&key) {
                if let Some(Modal::Select { filter, searching, selected, .. }) = &mut self.modal {
                    filter.push(c);
                    *searching = true;
                    *selected = 0;
                }
                self.finish_select_key(prev_selected, prev_tab, false);
                return;
            }
            if self.try_prefix(&["select"], &ks, KeyFocus::Select) || self.run_bindings(&["select"], &ks, false) {
                self.finish_select_key(prev_selected, prev_tab, false);
                return;
            }
        } else {
            let chain = select_chain(false, vim);
            if self.try_prefix(chain, &ks_folded, KeyFocus::Select) || self.run_bindings(chain, &ks_folded, false) {
                self.finish_select_key(prev_selected, prev_tab, false);
                return;
            }
        }

        if !searching
            && !allow_search
            && !multi
            && !vim
            && !approval
            && key.modifiers.is_empty()
            && let KeyCode::Char(raw) = key.code
            && let digit @ '1'..='9' = fold_fullwidth_ascii(raw)
            && let Some(Modal::Select { items, filter, reply, tab, filter_cache, .. }) = &mut self.modal
        {
            let visible = filter_cache.get(items, *tab, filter);
            let choices: Vec<usize> = visible.iter().copied().filter(|&i| items[i].is_selectable()).collect();
            let at = digit.to_digit(10).unwrap() as usize - 1;
            if choices.len() <= 9
                && let Some(&index) = choices.get(at)
                && let Some(position) = visible.iter().position(|&i| i == index)
            {
                send_select_reply(reply, items, &visible, (position, *tab), filter, None, false);
                self.pop_modal_or_promote_delayed();
                return;
            }
        }

        // Fallbacks: (a) vim count prefix, (b) start search with printable char, (c) Backspace, (d) drop.
        let mut keep_count = false;
        if let Some(Modal::Select { filter, searching, selected, count, vim, .. }) = &mut self.modal {
            if *vim
                && !*searching
                && !key.modifiers.intersects(
                    Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER | Modifiers::HYPER | Modifiers::META,
                )
                && let KeyCode::Char(raw) = key.code
                && let d @ '0'..='9' = fold_fullwidth_ascii(raw)
                && (d != '0' || *count > 0)
            {
                *count = (*count * 10 + d.to_digit(10).unwrap_or(0) as usize).min(9999);
                keep_count = true;
            } else if allow_search && let Some(c) = mare_tui::keys::printable_char(&key) {
                filter.push(c);
                *searching = true;
                *selected = 0;
            } else if key.code == KeyCode::Backspace {
                filter.pop();
                *selected = 0;
                if filter.is_empty() {
                    *searching = false;
                }
            }
        }
        self.finish_select_key(prev_selected, prev_tab, keep_count);
    }

    pub(super) fn finish_select_key(&mut self, prev_selected: usize, prev_tab: usize, keep_count: bool) {
        if let Some(Modal::Select { kind, items, filter, selected, tab, preview_scroll, count, filter_cache, .. }) =
            &mut self.modal
            && kind != "confirm"
        {
            if !keep_count {
                *count = 0;
            }
            let vis = filter_cache.get(items, *tab, filter);
            *selected = skip_unselectable(items, &vis, (*selected).min(vis.len().saturating_sub(1)), true);
            if *selected != prev_selected || *tab != prev_tab {
                *preview_scroll = 0;
            }
        }
    }

    /// Route modal core actions (`select.*`, `confirm.*`, `help.*`, `info.*`).
    /// Returns `None` for any unknown op or other action name so view/Lua actions can handle it.
    pub(super) fn modal_action(&mut self, action: &str) -> Option<bool> {
        if let Some(op) = action.strip_prefix("select.") {
            return self.run_select_action(op);
        }
        if let Some(op) = action.strip_prefix("confirm.") {
            return self.run_confirm_action(op);
        }
        if let Some(op) = action.strip_prefix("help.") {
            return self.run_help_action(op);
        }
        if let Some(op) = action.strip_prefix("info.") {
            return self.run_info_action(op);
        }
        None
    }

    fn run_confirm_action(&mut self, op: &str) -> Option<bool> {
        if !matches!(op, "yes" | "no" | "toggle" | "accept" | "cancel") && !is_scroll_op(op) {
            return None;
        }
        let Some(Modal::Select { kind, selected, body_scroll, body_scroll_max, reply, .. }) = &mut self.modal else {
            return Some(false);
        };
        let body_max = *body_scroll_max;
        if kind != "confirm" {
            return Some(false);
        }
        let send = |r: &mut Option<oneshot::Sender<Result<SelectReply, Dismissed>>>, i: Option<usize>| {
            if let Some(r) = r.take() {
                let _ = r.send(
                    i.map(|index| SelectReply {
                        index,
                        tab: 0,
                        key: None,
                        filter: String::new(),
                        ..Default::default()
                    })
                    .ok_or(Dismissed::Cancelled),
                );
            }
        };
        let mut close = false;
        match op {
            "yes" => {
                send(reply, Some(0));
                close = true;
            }
            "no" => {
                send(reply, Some(1));
                close = true;
            }
            "toggle" => *selected = 1 - (*selected).min(1),
            "accept" => {
                send(reply, Some(*selected));
                close = true;
            }
            "cancel" => {
                send(reply, None);
                close = true;
            }
            other if apply_scroll_op(other, body_scroll, body_max, 5) => {}
            _ => return None,
        }
        if close {
            self.pop_modal_or_promote_delayed();
        }
        Some(true)
    }

    fn run_select_action(&mut self, op: &str) -> Option<bool> {
        let known = matches!(
            op,
            "help"
                | "up"
                | "down"
                | "page_up"
                | "page_down"
                | "top"
                | "bottom"
                | "accept"
                | "back"
                | "search"
                | "tab_next"
                | "tab_prev"
                | "toggle"
                | "toggle_preview"
        ) || op.strip_prefix("preview_").is_some_and(is_scroll_op)
            || op.strip_prefix("body_").is_some_and(is_scroll_op);
        if !known {
            return None;
        }
        if op == "help" {
            if matches!(&self.modal, Some(Modal::Select { kind, .. }) if kind != "confirm") {
                self.pending_prefix = None;
                self.open_modal_help();
                return Some(true);
            }
            return Some(false);
        }
        let Some(Modal::Select {
            kind,
            items,
            filter,
            selected,
            reply,
            tabs,
            tab,
            searching,
            multi,
            allow_search,
            count,
            body_scroll,
            body_scroll_max,
            preview_scroll,
            preview_scroll_max,
            preview_hidden,
            filter_cache,
            ..
        }) = &mut self.modal
        else {
            return Some(false);
        };
        let body_max = *body_scroll_max;
        let preview_max = *preview_scroll_max;
        if kind == "confirm" {
            return Some(false);
        }
        let prev_selected = *selected;
        let prev_tab = *tab;
        let visible = filter_cache.get(items, *tab, filter);
        let n = visible.len();
        let step = (*count).max(1);
        let sel_pos: Vec<usize> = (0..n).filter(|&p| items[visible[p]].is_selectable()).collect();
        let mv = |sel: &mut usize, to: usize, down: bool| {
            *sel = skip_unselectable(items, &visible, to, down);
        };
        let mut close = false;
        if *multi && matches!(op, "up" | "down" | "page_up" | "page_down" | "top" | "bottom") {
            *searching = false;
        }
        match op {
            "up" | "down" => {
                if !sel_pos.is_empty() {
                    let m = sel_pos.len();
                    let down = op == "down";
                    let cur = sel_pos.iter().position(|&p| p == *selected);
                    *selected = match (cur, down) {
                        (Some(idx), true) => sel_pos[(idx + step) % m],
                        (Some(idx), false) => sel_pos[(idx + m - (step % m)) % m],
                        (None, true) => {
                            (*selected + 1..n).find(|&p| items[visible[p]].is_selectable()).unwrap_or(sel_pos[0])
                        }
                        (None, false) => {
                            (0..*selected).rev().find(|&p| items[visible[p]].is_selectable()).unwrap_or(sel_pos[m - 1])
                        }
                    };
                }
            }
            "page_up" => mv(selected, selected.saturating_sub(10), false),
            "page_down" => mv(selected, (*selected + 10).min(n.saturating_sub(1)), true),
            "top" => mv(selected, 0, true),
            "bottom" => mv(selected, n.saturating_sub(1), false),
            "accept" => {
                if *multi || visible.get(*selected).is_some_and(|&i| items[i].is_selectable()) {
                    send_select_reply(reply, items, &visible, (*selected, *tab), filter, None, *multi);
                    close = true;
                }
            }
            "back" => {
                if *searching {
                    filter.clear();
                    *searching = false;
                    *selected = 0;
                } else {
                    cancel_select_reply(reply);
                    close = true;
                }
            }
            "search" if *allow_search => *searching = true,
            "search" => return Some(false),
            "toggle" if *multi && !*searching => {
                if let Some(&i) = visible.get(*selected).filter(|&&i| items[i].is_selectable()) {
                    items[i].checked = !items[i].checked;
                }
            }
            "toggle" => return Some(false),
            "toggle_preview" => *preview_hidden = !*preview_hidden,
            "tab_next" | "tab_prev" => {
                let n_tabs = tabs.len();
                if n_tabs < 2 {
                    return Some(false);
                }
                let cur = (*tab).min(n_tabs - 1);
                let next = if op == "tab_next" { cur + 1 } else { cur + n_tabs - 1 };
                switch_select_tab(items, selected, filter, tabs, tab, next);
                filter_cache.bump();
            }
            _ if op.strip_prefix("preview_").is_some_and(|sub| {
                let (scroll, max) = if preview_max.is_some_and(|m| m > 0) {
                    (&mut *preview_scroll, preview_max)
                } else {
                    (&mut *body_scroll, body_max)
                };
                apply_scroll_op(sub, scroll, max, 5)
            }) => {}
            _ if op.strip_prefix("body_").is_some_and(|sub| {
                let (scroll, max) = if body_max.is_some_and(|m| m > 0) {
                    (&mut *body_scroll, body_max)
                } else {
                    (&mut *preview_scroll, preview_max)
                };
                apply_scroll_op(sub, scroll, max, 5)
            }) => {}
            _ => return None,
        }
        if close {
            self.pending_prefix = None;
            self.pop_modal_or_promote_delayed();
        } else {
            // The key handler clears a consumed count after the action returns.
            self.finish_select_key(prev_selected, prev_tab, true);
        }
        Some(true)
    }

    fn run_help_action(&mut self, op: &str) -> Option<bool> {
        if !matches!(op, "up" | "down" | "accept" | "cancel") {
            return None;
        }
        let picked = {
            let Some(Modal::Help { items, filter, selected, .. }) = &mut self.modal else {
                return Some(false);
            };
            let visible = filter_items(items, filter);
            match op {
                "up" => {
                    let start = selected.checked_sub(1).unwrap_or(visible.len().saturating_sub(1));
                    *selected = skip_unselectable(items, &visible, start, false);
                    return Some(true);
                }
                "down" => {
                    *selected = skip_unselectable(items, &visible, (*selected + 1) % visible.len().max(1), true);
                    return Some(true);
                }
                "accept" => match visible.get(*selected).copied().filter(|&i| items[i].is_selectable()) {
                    Some(index) => Some(Some(index)),
                    None => return Some(true),
                },
                "cancel" => Some(None),
                _ => return None,
            }
        };
        if let Some(choice) = picked {
            self.dispatch_help_choice(choice);
        }
        Some(true)
    }

    fn run_info_action(&mut self, op: &str) -> Option<bool> {
        if op == "close" {
            if matches!(self.modal, Some(Modal::Info { .. })) {
                self.pop_modal_or_promote_delayed();
                return Some(true);
            }
            return Some(false);
        }
        if !is_scroll_op(op) {
            return None;
        }
        let Some(Modal::Info { scroll, scroll_max, .. }) = &mut self.modal else {
            return Some(false);
        };
        apply_scroll_op(op, scroll, *scroll_max, 10);
        Some(true)
    }

    fn dispatch_help_choice(&mut self, choice: Option<usize>) {
        let Some(Modal::Help { acts, .. }) = self.modal.take() else { return };
        self.modal = self.modal_stack.pop();
        let mut acts = acts;
        if let Some(i) = choice
            && i < acts.len()
        {
            match acts.swap_remove(i) {
                HelpAct::Group => {}
                HelpAct::Key(k) if self.modal.is_some() => self.handle_modal_key(k),
                HelpAct::Key(k) => self.handle_key(k),
                HelpAct::Action(a) => {
                    self.run_action(&a);
                }
            }
        }
    }
}

fn switch_select_tab(
    items: &mut Vec<SelectItem>,
    selected: &mut usize,
    filter: &mut String,
    tabs: &mut [SelectTab],
    tab: &mut usize,
    next: usize,
) {
    if tabs.len() < 2 {
        return;
    }
    let cur = (*tab).min(tabs.len() - 1);
    if let Some(prev) = tabs.get_mut(cur) {
        prev.items = std::mem::take(items);
        prev.selected = *selected;
    }
    *tab = next % tabs.len();
    if let Some(dest) = tabs.get_mut(*tab) {
        *items = std::mem::take(&mut dest.items);
        *selected = dest.selected;
    }
    filter.clear();
}

fn cancel_select_reply(reply: &mut Option<oneshot::Sender<Result<SelectReply, Dismissed>>>) {
    if let Some(r) = reply.take() {
        let _ = r.send(Err(Dismissed::Cancelled));
    }
}

fn send_select_reply(
    reply: &mut Option<oneshot::Sender<Result<SelectReply, Dismissed>>>,
    items: &[SelectItem],
    visible: &[usize],
    position: (usize, usize),
    filter: &str,
    key: Option<char>,
    multi: bool,
) {
    let (selected, tab) = position;
    if let Some(r) = reply.take() {
        let index = visible.get(selected).copied().filter(|&i| items[i].is_selectable());
        let pick = index.or(multi.then_some(0)).map(|index| SelectReply {
            index,
            selected_indices: if multi {
                items
                    .iter()
                    .enumerate()
                    .filter_map(|(i, item)| (item.checked && item.is_selectable()).then_some(i))
                    .collect()
            } else {
                Vec::new()
            },
            tab,
            key,
            filter: filter.to_string(),
        });
        let _ = r.send(pick.ok_or(Dismissed::Cancelled));
    }
}

impl App {
    fn handle_input_modal_key(&mut self, key: KeyEvent) {
        let ks = key_to_string(&key);
        let printable = mare_shared::bindings::is_printable_key_name(&ks);
        if self.dispatch_bindings_stops(&["input", "text"], &ks, printable) {
            return;
        }
        if let Some(c) = mare_tui::keys::printable_char(&key)
            && let Some(Modal::Input { editor, .. }) = self.modal.as_mut()
        {
            editor.insert_char(c);
        }
    }
}
