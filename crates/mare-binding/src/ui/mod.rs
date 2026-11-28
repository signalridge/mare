//! `mare.ui`: the Lua-facing UI API. Plugins build component trees (plain tables built by
//! `ui.Line`, `ui.Span`, ... from the prelude), the core converts and caches them; the TUI
//! renders cached nodes every frame without calling back into Lua.

mod chrome;
pub mod completion;
mod modals;
mod surface;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use mlua::{Lua, Table, Value};

pub use mare_shared::ui::{BlockPatch, Node, SelectItem, SelectTab, UiCmd, UiHooks, UiShared};

pub(crate) use crate::ui_node::{line_value, spans_from};
pub use crate::ui_node::{node_from, style_from};
pub use modals::{non_empty_str, split_reason_parts, title_first_line};
pub(crate) use modals::{parse_cancel, wait_input, wait_select};

/// Parse a non-negative integer Lua number/integer (rejecting negative, fractional, or NaN values).
pub(super) fn non_neg_int(v: Value, context: &str) -> mlua::Result<usize> {
    match v {
        Value::Integer(n) if n >= 0 => Ok(n as usize),
        Value::Number(n) if n.is_finite() && n >= 0.0 && n.fract() == 0.0 => Ok(n as usize),
        _ => Err(mlua::Error::runtime(format!("{context}: expected a non-negative integer"))),
    }
}

/// Render a Lua UI node (`ui.Line`, `ui.Markdown`, string, etc.) to plain text for RPC forwarding.
pub fn render_node_text(shared: &UiShared, v: Value, width: usize) -> mlua::Result<String> {
    let theme = shared.theme.read().unwrap().clone();
    let node = node_from(&theme, v)?;
    let lines = shared.render(&node, width);
    Ok(lines.iter().map(|l| l.plain()).collect::<Vec<_>>().join("\n"))
}

/// Render a Lua UI line (`ui.Line`, spans, or string) to plain text for RPC forwarding.
pub fn render_line_text(shared: &UiShared, v: Value) -> mlua::Result<String> {
    let theme = shared.theme.read().unwrap().clone();
    Ok(line_value(&theme, v)?.plain())
}

fn patch_from(_lua: &Lua, shared: &UiShared, t: &Table) -> mlua::Result<BlockPatch> {
    let theme = shared.theme.read().unwrap().clone();
    let mut p = BlockPatch::default();
    if let Some(v) = t.get::<Option<Value>>("node")? {
        p.node = Some(node_from(&theme, v)?);
    }
    if let Some(v) = t.get::<Option<Value>>("expanded")? {
        p.expanded = Some(node_from(&theme, v)?);
    }
    p.title = t.get("title")?;
    p.markdown = t.get("markdown")?;
    p.append = t.get("append")?;
    p.kind = t.get("kind")?;
    Ok(p)
}

/// Install the UI functions into `ui` (the table built by the preset's ui_prelude.lua).
pub fn install(lua: &Lua, mare: &Table, ui: Table, shared: Arc<UiShared>) -> mlua::Result<()> {
    crate::ui_wire::install(lua, &ui)?;
    crate::ui_image_resource::install(lua, &ui)?;
    surface::install(lua, &ui, &shared)?;
    modals::install(lua, &ui, &shared)?;
    chrome::install(lua, &ui, &shared)?;
    completion::install(lua, &ui, &shared)?;
    mare.set("ui", ui)?;
    Ok(())
}
