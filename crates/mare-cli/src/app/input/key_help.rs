//! The `?` key menus and key-conflict warnings.

use crate::app::view::modal::HelpAct;
use crate::app::*;

pub(in crate::app) type BindingDescriptions =
    (std::collections::HashMap<String, String>, std::collections::HashMap<(String, String, String), String>);

impl App {
    pub(in crate::app) fn binding_descriptions(&self) -> BindingDescriptions {
        let mut descriptions: std::collections::HashMap<String, String> = mare_shared::bindings::CORE_ACTIONS
            .iter()
            .map(|action| (action.name.to_string(), action.desc.to_string()))
            .collect();
        let mut per_binding: std::collections::HashMap<(String, String, String), String> =
            std::collections::HashMap::new();
        let read = || -> mlua::Result<(mlua::Table, Option<mlua::Table>)> {
            let mare: mlua::Table = self.host.lua().globals().get("mare")?;
            let keymap: mlua::Table = mare.get("keymap")?;
            let actions = keymap.get::<mlua::Function>("actions")?.call(())?;
            let maps = mare_shared::lua::core::keymap::maps(self.host.lua())?;
            Ok((actions, maps))
        };
        if let Ok((actions, maps)) = read() {
            for action in actions.sequence_values::<mlua::Table>().flatten() {
                if let (Ok(name), Ok(Some(desc))) = (action.get::<String>("name"), action.get::<Option<String>>("desc"))
                {
                    descriptions.insert(name, desc);
                }
            }
            if let Some(maps) = maps {
                for (key, stack) in maps.pairs::<String, mlua::Table>().flatten() {
                    for entry in stack.sequence_values::<mlua::Table>().flatten() {
                        if let (Ok(ctx), Ok(action), Ok(Some(desc))) = (
                            entry.get::<String>("context"),
                            entry.get::<String>("action"),
                            entry.get::<Option<String>>("desc"),
                        ) && !desc.is_empty()
                        {
                            descriptions.entry(action.clone()).or_insert_with(|| desc.clone());
                            per_binding.insert((ctx, key.clone(), action), desc);
                        }
                    }
                }
            }
        }
        (descriptions, per_binding)
    }

    /// `?` inside a list overlay: rows from live bindings of its context chain; Enter runs the action.
    pub(super) fn open_modal_help(&mut self) {
        if self.select.active() {
            self.leave_select_mode();
        }
        let vim = matches!(&self.modal, Some(Modal::Select { vim: true, .. }));
        let allow_search = !matches!(&self.modal, Some(Modal::Select { allow_search: false, .. }));
        let chain = super::select_chain(false, vim);
        let mut items: Vec<SelectItem> = Vec::new();
        let mut acts: Vec<HelpAct> = Vec::new();
        let multi = matches!(&self.modal, Some(Modal::Select { multi: true, .. }));
        let mut seen = std::collections::HashSet::new();
        let mut push_action = |action: &str, desc: String| {
            let keys = self.keymap.hint(action, chain);
            if keys.is_empty() {
                return;
            }
            let label = keys.into_iter().map(mare_shared::bindings::key_label).collect::<Vec<_>>().join("/");
            items.push(SelectItem { label, detail: Some(desc), ..Default::default() });
            acts.push(HelpAct::Action(action.to_string()));
        };
        if multi && let Some(action) = mare_shared::bindings::find_core_action("select.toggle") {
            seen.insert(action.name.to_string());
            push_action(action.name, action.desc.to_string());
        }
        for a in mare_shared::bindings::CORE_ACTIONS {
            if a.name.starts_with("select.")
                && (allow_search || a.name != "select.search")
                && (multi || a.name != "select.toggle")
                && seen.insert(a.name.to_string())
            {
                push_action(a.name, a.desc.to_string());
            }
        }
        let (descriptions, per_binding) = self.binding_descriptions();
        for (ctx, key, entry) in self.keymap.reachable_in_chain(chain) {
            let action = entry.action.clone();
            if (allow_search || action != "select.search")
                && (multi || action != "select.toggle")
                && seen.insert(action.clone())
            {
                let desc = per_binding
                    .get(&(ctx.to_string(), key.to_string(), action.clone()))
                    .cloned()
                    .unwrap_or_else(|| action_description(&descriptions, &action));
                push_action(&action, desc);
            }
        }
        if let Some(m) = self.modal.take() {
            self.modal_stack.push(m);
        }
        self.reset_select_row_failures();
        self.modal = Some(Modal::Help { items, acts, filter: String::new(), selected: 0 });
    }

    /// `?` on an empty input: every key binding, searchable; Enter runs it.
    /// Bindings that a terminal / multiplexer is known to take first.
    fn key_conflicts(&self) -> Vec<mare_tui::keyconflict::Conflict> {
        let mut binds: Vec<(String, String)> =
            self.keymap.bindable_contexts().into_iter().flat_map(|c| self.keymap.bindings(c)).collect();
        binds.sort();
        binds.dedup_by(|a, b| a.0 == b.0);
        let terms = mare_tui::keyconflict::terminal_id(&|k| std::env::var(k).ok());
        mare_tui::keyconflict::find(&binds, &self.conflict_table, &terms)
    }

    /// Bindings that can never be typed (a key that also starts a sequence).
    pub(in crate::app) fn ambiguous_keys(&self) -> Vec<String> {
        self.keymap
            .bindable_contexts()
            .into_iter()
            .flat_map(|c| mare_tui::keyconflict::ambiguous(&self.keymap.bindings(c)))
            .collect()
    }

    /// Startup: tell the user once per distinct set of conflicts.
    pub(in crate::app) fn warn_key_conflicts(&mut self) {
        let c = self.key_conflicts();
        let path = mare_shared::paths::state_dir().join("key-conflicts");
        let sig = mare_tui::keyconflict::signature(&c);
        if c.is_empty() || std::fs::read_to_string(&path).ok().as_deref() == Some(sig.as_str()) {
            return;
        }
        let _ = std::fs::write(&path, &sig);
        let list: Vec<String> = c.iter().map(|c| format!("{} ({})", c.key, c.by)).collect();
        self.push_notice((
            format!("Some key bindings may never reach mare: {} — `?` shows how to fix them", list.join(", ")),
            "warning".into(),
            Instant::now(),
        ));
    }

    pub(super) fn open_keymap_help(&mut self) {
        let chain = self.editor_chain(false);
        let chain_refs: Vec<&str> = chain.iter().map(String::as_str).collect();
        let binds = self.keymap.reachable_in_chain(&chain_refs);
        let (descriptions, per_binding) = self.binding_descriptions();
        let conflicts = self.key_conflicts();
        let reserved = mare_tui::keys::ctrl_c_event();
        let mut items = vec![
            SelectItem { label: "Shortcuts".into(), header: true, ..Default::default() },
            SelectItem {
                label: mare_shared::bindings::key_label(&key_to_string(&reserved)),
                detail: Some("interrupt / clear input · twice to exit (reserved)".into()),
                ..Default::default()
            },
        ];
        let mut acts = vec![HelpAct::Group, HelpAct::Key(reserved)];
        let mut entries = [Vec::new(), Vec::new()];
        for (ctx, key, entry) in binds {
            let action = entry.action.clone();
            let mut detail = per_binding
                .get(&(ctx.to_string(), key.to_string(), action.clone()))
                .cloned()
                .unwrap_or_else(|| action_description(&descriptions, &action));
            if !entry.when.always() {
                let _ = std::fmt::Write::write_fmt(
                    &mut detail,
                    format_args!(" [when: {}]", entry.when.labels().join(", ")),
                );
            }
            if let Some(c) = conflicts.iter().find(|c| c.key == key) {
                let _ = std::fmt::Write::write_fmt(&mut detail, format_args!("  ⚠ taken by {} — {}", c.by, c.fix));
            }
            let group = usize::from(action.starts_with("text.") || action == "editor.submit");
            let label = key.split_whitespace().map(mare_shared::bindings::key_label).collect::<Vec<_>>().join(" ");
            entries[group]
                .push((SelectItem { label, detail: Some(detail), ..Default::default() }, HelpAct::Action(action)));
        }
        for (group, entries) in entries.into_iter().enumerate() {
            if group == 1 && !entries.is_empty() {
                items.push(SelectItem { label: "Input editor".into(), header: true, ..Default::default() });
                acts.push(HelpAct::Group);
            }
            for (item, action) in entries {
                items.push(item);
                acts.push(action);
            }
        }
        if self.select.active() {
            self.leave_select_mode();
        }
        self.reset_select_row_failures();
        self.modal = Some(Modal::Help { items, acts, filter: String::new(), selected: 1 });
    }
}

pub(in crate::app) fn action_description(
    descriptions: &std::collections::HashMap<String, String>,
    action: &str,
) -> String {
    descriptions.get(action).cloned().unwrap_or_else(|| {
        if action.starts_with("fn:") { "Lua function".to_string() } else { action.replace(['-', '_', '.'], " ") }
    })
}
