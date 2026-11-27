//! `mare.ui.select`'s items from Lua, and the handle `on_open` gets: `sel:update(items)` replaces
//! the open select's items in place (`UiCmd::SelectUpdate`), keeping the search text, the
//! searching state and the highlighted item (found again by `item_id`, else by label).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mlua::{Table, UserData, UserDataMethods, Value};

use mare_shared::style::Theme;
use mare_shared::ui::{SelectItem, SelectTab, UiCmd, UiShared};

use crate::ui::{line_value, node_from};

/// `{string | {label, detail?, preview?, header?, disabled?, item_id?, lines?}}` → items (`item_id`: a string or a
/// number; `lines`: rows drawn instead of `label  detail`, each a `ui.Line`, spans or a string).
pub(crate) fn parse_items(theme: &Theme, list: Option<Table>) -> mlua::Result<Vec<SelectItem>> {
    let mut items = vec![];
    for v in list.into_iter().flat_map(|l| l.sequence_values::<Value>().collect::<Vec<_>>()) {
        match v? {
            Value::String(s) => items.push(SelectItem { label: s.to_str()?.to_string(), ..Default::default() }),
            Value::Table(it) => items.push(SelectItem {
                label: it.get::<Option<String>>("label")?.unwrap_or_default(),
                detail: it.get("detail")?,
                preview: match it.get::<Value>("preview")? {
                    Value::Nil => None,
                    v => Some(node_from(theme, v)?),
                },
                header: it.get::<Option<bool>>("header")?.unwrap_or(false),
                disabled: it.get::<Option<bool>>("disabled")?.unwrap_or(false),
                checked: it.get::<Option<bool>>("checked")?.unwrap_or(false),
                item_id: match it.get::<Value>("item_id")? {
                    Value::String(s) => Some(s.to_str()?.to_string()),
                    Value::Integer(n) => Some(n.to_string()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                },
                lines: match it.get::<Option<Table>>("lines")? {
                    Some(l) => {
                        l.sequence_values::<Value>().map(|v| line_value(theme, v?)).collect::<mlua::Result<_>>()?
                    }
                    None => vec![],
                },
            }),
            _ => {}
        }
    }
    Ok(items)
}

/// `{ {name, items, selected?}, … }` → tabs (`selected` 1-based).
pub(crate) fn parse_tabs(theme: &Theme, list: Option<Table>) -> mlua::Result<Vec<SelectTab>> {
    let mut tabs = vec![];
    for v in list.into_iter().flat_map(|l| l.sequence_values::<Table>().collect::<Vec<_>>()) {
        let v = v?;
        tabs.push(SelectTab {
            name: v.get::<Option<String>>("name")?.unwrap_or_default(),
            items: parse_items(theme, v.get("items")?)?,
            selected: v.get::<Option<usize>>("selected")?.unwrap_or(1).saturating_sub(1),
        });
    }
    Ok(tabs)
}

/// The handle of one open select (`mare.ui.select{on_open = function(sel) … end}`).
pub(crate) struct SelectHandle {
    pub(crate) token: u64,
    pub(crate) ui: Arc<UiShared>,
    /// false once the select has been answered or closed
    pub(crate) open: Arc<AtomicBool>,
}

impl UserData for SelectHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // update(items) | update{items = …} | update{tabs = …} -> still open
        methods.add_method("update", |_, this, t: Table| {
            if !this.open.load(Ordering::SeqCst) {
                return Ok(false);
            }
            let theme = this.ui.theme.read().unwrap().clone();
            let spec = t.contains_key("items")? || t.contains_key("tabs")?;
            let (items, tabs) = if spec {
                (parse_items(&theme, t.get("items")?)?, parse_tabs(&theme, t.get("tabs")?)?)
            } else {
                (parse_items(&theme, Some(t))?, vec![])
            };
            Ok(this.ui.send(UiCmd::SelectUpdate { token: this.token, items, tabs }))
        });
        methods.add_method("is_open", |_, this, ()| Ok(this.open.load(Ordering::SeqCst)));
    }
}
