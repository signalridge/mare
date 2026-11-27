//! Theme, keymap, editor bar, and window-chrome bindings for `mare.ui`.

use std::collections::HashMap;
use std::sync::Arc;

use mlua::{Lua, LuaSerdeExt, Table, Value};

use mare_shared::bindings::{CORE_ACTIONS, Entry, When};
use mare_shared::style::{Color, ColorMode, no_color_env};
use mare_shared::ui::{EditorStateSnapshot, EditorSyncOp};

use super::{UiCmd, UiShared, non_neg_int, style_from};

fn pos_to_table(lua: &Lua, (row, col): (usize, usize)) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("row", row)?;
    t.set("col", col)?;
    Ok(t)
}

fn parse_pos(v: &Value, label: &str) -> mlua::Result<(usize, usize)> {
    let Value::Table(t) = v else {
        return Err(mlua::Error::runtime(format!("{label}: expected {{ row = ..., col = ... }}")));
    };
    let row_val: Value = t.get("row")?;
    let col_val: Value = t.get("col")?;
    let (r, c) = if row_val.is_nil() && col_val.is_nil() {
        (t.get::<Value>(1)?, t.get::<Value>(2)?)
    } else {
        (row_val, col_val)
    };
    Ok((non_neg_int(r, label)?, non_neg_int(c, label)?))
}

fn selection_to_value(lua: &Lua, snap: &EditorStateSnapshot) -> mlua::Result<Value> {
    let (Some((sr, sc)), Some((er, ec))) = (snap.selection.map(|x| x.0), snap.selection.map(|x| x.1)) else {
        return Ok(Value::Nil);
    };
    let t = lua.create_table()?;
    t.set("start", pos_to_table(lua, (sr, sc))?)?;
    t.set("end", pos_to_table(lua, (er, ec))?)?;
    let anchor = snap.anchor.unwrap_or((sr, sc));
    let head = snap.head.unwrap_or((er, ec));
    t.set("anchor", pos_to_table(lua, anchor)?)?;
    t.set("head", pos_to_table(lua, head)?)?;
    if let Some(ref txt) = snap.selected_text {
        t.set("text", txt.as_str())?;
    }
    Ok(Value::Table(t))
}

fn snapshot_to_table(lua: &Lua, snap: &EditorStateSnapshot) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("text", snap.text.as_str())?;
    t.set("cursor", pos_to_table(lua, snap.cursor)?)?;
    t.set("selection", selection_to_value(lua, snap)?)?;
    t.set("empty", snap.empty)?;
    t.set("revision", snap.revision)?;
    Ok(t)
}

fn read_mare_option(lua: &Lua, path: &[&str]) -> Option<String> {
    let mare: Table = lua.globals().get("mare").ok()?;
    let mut cur: Table = mare.get("o").or_else(|_| mare.get("config")).ok()?;
    for &seg in &path[..path.len().saturating_sub(1)] {
        cur = cur.get(seg).ok()?;
    }
    path.last().and_then(|k| cur.get::<Option<String>>(*k).ok().flatten())
}

fn resolve_color_mode(lua: &Lua, shared: &UiShared, option: Option<&str>) -> ColorMode {
    let opt = option.map(str::to_owned).or_else(|| read_mare_option(lua, &["terminal", "color"]));
    let env = |k: &str| std::env::var(k).ok();
    let caps = shared.caps.read().unwrap();
    let probed_tc = caps.get("probed_truecolor").and_then(|v| v.as_bool());
    if option.and_then(ColorMode::parse).is_none()
        && opt.as_deref().and_then(ColorMode::parse).is_none()
        && !no_color_env(&env)
        && let Some(override_mode) =
            caps.get("color_depth_override").and_then(|v| v.as_str()).and_then(ColorMode::parse)
    {
        return override_mode;
    }
    ColorMode::resolve(opt.as_deref(), &env, probed_tc)
}

fn normalize_seq_with(shared: &UiShared, key: &str) -> String {
    if key.contains(' ') {
        let parts: Vec<String> = key.split_whitespace().map(|t| shared.normalize_key(t)).collect();
        if parts.len() >= 2 {
            return parts.join(" ");
        }
        return shared.normalize_key(key);
    }
    if !key.contains('<') {
        return shared.normalize_key(key);
    }
    let mut tokens = Vec::new();
    let mut rest = key;
    while !rest.is_empty() {
        if rest.starts_with('<') {
            // Find the closing `>` of this `<...>` key notation, skipping a literal `>` key right after `-`/`+` (e.g. `<C->>`).
            let bytes = rest.as_bytes();
            let mut close = None;
            let mut idx = 1;
            while idx < bytes.len() {
                if bytes[idx] == b'>' {
                    let is_mod_literal_gt =
                        idx >= 2 && matches!(bytes[idx - 1], b'-' | b'+') && bytes.get(idx + 1) == Some(&b'>');
                    if !is_mod_literal_gt {
                        close = Some(idx);
                        break;
                    }
                }
                idx += 1;
            }
            if let Some(end) = close {
                tokens.push(shared.normalize_key(&rest[..=end]));
                rest = &rest[end + 1..];
                continue;
            }
        }
        let ch_len = rest.chars().next().map_or(1, char::len_utf8);
        tokens.push(shared.normalize_key(&rest[..ch_len]));
        rest = &rest[ch_len..];
    }
    if tokens.len() <= 1 { shared.normalize_key(key) } else { tokens.join(" ") }
}

pub(super) fn install(lua: &Lua, ui: &Table, shared: &Arc<UiShared>) -> mlua::Result<()> {
    macro_rules! sh {
        () => {
            shared.clone()
        };
    }

    // set_theme_data({ border?, glyphs?, icons?, markdown? }): interface-shell theme & glyph data
    // . Omitted (nil) fields stay unchanged; passing `glyphs = false` or `{}`
    // restores the built-in spinner glyphs.
    let s = sh!();
    ui.set(
        "set_theme_data",
        lua.create_function(move |_, opts: Option<Table>| {
            let Some(o) = opts else {
                return Ok(());
            };
            let mut changed = false;
            let mut t = s.theme.write().unwrap();
            if let Some(name) = o.get::<Option<String>>("border")? {
                t.border = name;
                changed = true;
            }
            if let Some(name) = o.get::<Option<String>>("icons")? {
                t.icons = name;
                changed = true;
            }
            match o.get::<Value>("glyphs")? {
                Value::Nil => {}
                Value::Boolean(false) => {
                    t.spinner = mare_shared::style::SPINNER_FRAMES.iter().map(|g| g.to_string()).collect();
                    t.spinner_still = mare_shared::style::SPINNER_STILL.to_string();
                    changed = true;
                }
                Value::Table(g) => {
                    let frames: Option<Vec<String>> = match g.get::<Value>("frames")? {
                        Value::Table(ft) => Some(ft.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>()?),
                        Value::Nil if g.raw_len() > 0 => {
                            Some(g.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>()?)
                        }
                        _ => None,
                    };
                    let still = g.get::<Option<String>>("still")?;
                    let frames = frames.filter(|f| !f.is_empty() && f.iter().all(|x| !x.is_empty()));
                    t.spinner = frames
                        .unwrap_or_else(|| mare_shared::style::SPINNER_FRAMES.iter().map(|x| x.to_string()).collect());
                    t.spinner_still = still
                        .filter(|x| !x.is_empty())
                        .unwrap_or_else(|| mare_shared::style::SPINNER_STILL.to_string());
                    changed = true;
                }
                _ => {
                    return Err(mlua::Error::RuntimeError("ui.set_theme_data: glyphs must be a table or false".into()));
                }
            }
            if let Some(md) = o.get::<Option<Table>>("markdown")? {
                let mut glyphs = HashMap::new();
                if let Ok(Some(g)) = md.get::<Option<Table>>("glyphs") {
                    for (k, v) in g.pairs::<String, String>().flatten() {
                        glyphs.insert(k, v);
                    }
                }
                let numbers = md.get::<Option<bool>>("code_line_numbers").ok().flatten().unwrap_or(false);
                t.md.glyphs = glyphs;
                t.md.code_line_numbers = numbers;
                changed = true;
            }
            drop(t);
            if changed {
                mare_shared::style::theme_changed();
                s.send(UiCmd::ThemeChanged);
            }
            Ok(())
        })?,
    )?;
    let s = sh!();
    ui.set("icon_set", lua.create_function(move |_, ()| Ok(s.theme.read().unwrap().icons.clone()))?)?;
    let s_caps = sh!();
    ui.set(
        "color_depth",
        lua.create_function(move |lua, ()| Ok(resolve_color_mode(lua, &s_caps, None).as_str().to_string()))?,
    )?;
    let s_caps = sh!();
    ui.set(
        "detect_icons",
        lua.create_function(move |lua, caps: Option<Table>| {
            let opt = read_mare_option(lua, &["icons"]);
            let emoji_wide = caps
                .and_then(|t| t.get::<Option<bool>>("emoji_wide").ok().flatten())
                .or_else(|| s_caps.caps.read().unwrap().get("emoji_wide").and_then(|v| v.as_bool()));
            let env = |k: &str| std::env::var(k).ok();
            Ok(mare_shared::style::resolve_icons(opt.as_deref(), &env, emoji_wide).to_string())
        })?,
    )?;
    let s = sh!();
    ui.set(
        "set_color_mode",
        lua.create_function(move |lua, option: Option<String>| {
            let mode = resolve_color_mode(lua, &s, option.as_deref());
            if let Ok(mut caps) = s.caps.write() {
                caps["color_depth"] = serde_json::json!(mode.as_str());
                caps["truecolor"] = serde_json::json!(mode == mare_shared::style::ColorMode::TrueColor);
                caps["no_color"] = serde_json::json!(mode == mare_shared::style::ColorMode::NoColor);
                if option.as_deref().and_then(mare_shared::style::ColorMode::parse).is_some()
                    && let Some(obj) = caps.as_object_mut()
                {
                    obj.remove("color_depth_override");
                }
            }
            s.send(UiCmd::ColorMode(mode));
            Ok(mode.as_str().to_string())
        })?,
    )?;
    // set_ambiwidth("single"|"double")
    let s = sh!();
    ui.set(
        "set_ambiwidth",
        lua.create_function(move |_, name: String| {
            let double = match name.as_str() {
                "single" => false,
                "double" => true,
                _ => {
                    return Err(mlua::Error::runtime(format!(
                        "ui.set_ambiwidth: expected \"single\" or \"double\", got \"{name}\""
                    )));
                }
            };
            mare_shared::style::set_ambiwidth_double(double);
            mare_shared::style::theme_changed();
            s.send(UiCmd::ThemeChanged);
            Ok(())
        })?,
    )?;
    // set_title(text): terminal window title (OSC 2); no-op without a TUI
    let s = sh!();
    ui.set("set_title", lua.create_function(move |_, text: String| Ok(s.send(UiCmd::Title { text })))?)?;

    let s = sh!();
    ui.set(
        "set_editor",
        // set_editor(text, {keep_draft = true}): keep_draft leaves a non-empty input untouched;
        // {prepend = true} puts the text before it (separated by a blank line).
        // images carries the complete draft attachment payload in marker order; IDs are
        // remapped when needed to preserve attachments already in the editor.
        lua.create_function(move |lua, (text, opts): (String, Option<Table>)| {
            let flag = |k: &str| opts.as_ref().and_then(|o| o.get::<Option<bool>>(k).ok().flatten()).unwrap_or(false);
            let (keep_draft, prepend) = (flag("keep_draft"), flag("prepend"));
            let images: Vec<serde_json::Value> = opts
                .as_ref()
                .map(|o| o.get::<Option<Value>>("images"))
                .transpose()?
                .flatten()
                .map(|v| lua.from_value(v))
                .transpose()?
                .unwrap_or_default();
            let image_markers: Vec<serde_json::Value> = opts
                .as_ref()
                .map(|o| o.get::<Option<Value>>("image_markers"))
                .transpose()?
                .flatten()
                .map(|v| lua.from_value(v))
                .transpose()?
                .unwrap_or_default();
            if !keep_draft && !prepend && images.is_empty() && image_markers.is_empty() {
                s.apply_editor_op(EditorSyncOp::SetText(text)).map_err(mlua::Error::runtime)?;
                return Ok(());
            }
            s.send(UiCmd::SetEditor { text, images, image_markers, keep_draft, prepend });
            Ok(())
        })?,
    )?;
    // get_editor() -> string: current raw editor text.
    let s = sh!();
    ui.set("get_editor", lua.create_function(move |_, ()| Ok(s.read_editor_snapshot().text))?)?;
    // editor_state() -> { text, cursor = {row, col}, selection = ... | nil, empty, revision }
    let s = sh!();
    ui.set("editor_state", lua.create_function(move |lua, ()| snapshot_to_table(lua, &s.read_editor_snapshot()))?)?;
    // editor_cursor() -> { row, col }
    let s = sh!();
    ui.set("editor_cursor", lua.create_function(move |lua, ()| pos_to_table(lua, s.read_editor_snapshot().cursor))?)?;
    // set_editor_cursor(row, col) | set_editor_cursor({ row, col })
    let s = sh!();
    ui.set(
        "set_editor_cursor",
        lua.create_function(move |_, (first, second): (Value, Option<Value>)| {
            let (row, col) = match (&first, second) {
                (Value::Table(_), None) => parse_pos(&first, "mare.ui.set_editor_cursor")?,
                (_, Some(c)) => {
                    (non_neg_int(first, "mare.ui.set_editor_cursor")?, non_neg_int(c, "mare.ui.set_editor_cursor")?)
                }
                _ => {
                    return Err(mlua::Error::runtime("mare.ui.set_editor_cursor: expected (row, col) or { row, col }"));
                }
            };
            s.apply_editor_op(EditorSyncOp::SetCursor { row, col }).map_err(mlua::Error::runtime)?;
            Ok(())
        })?,
    )?;
    // editor_selection() -> { start, end, anchor, head, text } | nil
    let s = sh!();
    ui.set(
        "editor_selection",
        lua.create_function(move |lua, ()| selection_to_value(lua, &s.read_editor_snapshot()))?,
    )?;
    // set_editor_selection(nil) | set_editor_selection(anchor, head) | set_editor_selection({ anchor, head })
    let s = sh!();
    ui.set(
        "set_editor_selection",
        lua.create_function(move |_, (first, second): (Value, Option<Value>)| {
            let (anchor, head) = match (first, second) {
                (Value::Nil, _) => (None, None),
                (Value::Table(t), None) => {
                    let a: Value = t.get("anchor")?;
                    let h: Value = t.get("head")?;
                    if a.is_nil() && h.is_nil() {
                        if t.pairs::<Value, Value>().next().is_some() {
                            return Err(mlua::Error::runtime(
                                "mare.ui.set_editor_selection: table must provide anchor/head or be empty",
                            ));
                        }
                        (None, None)
                    } else {
                        (
                            Some(parse_pos(&a, "mare.ui.set_editor_selection")?),
                            Some(parse_pos(&h, "mare.ui.set_editor_selection")?),
                        )
                    }
                }
                (a, Some(Value::Nil)) => {
                    let _ = a;
                    (None, None)
                }
                (a, Some(h)) => (
                    Some(parse_pos(&a, "mare.ui.set_editor_selection")?),
                    Some(parse_pos(&h, "mare.ui.set_editor_selection")?),
                ),
                (_, None) => {
                    return Err(mlua::Error::runtime(
                        "mare.ui.set_editor_selection: expected nil, (anchor, head), or { anchor = ..., head = ... }",
                    ));
                }
            };
            s.apply_editor_op(EditorSyncOp::SetSelection { anchor, head }).map_err(mlua::Error::runtime)?;
            Ok(())
        })?,
    )?;
    // editor_insert(text)
    let s = sh!();
    ui.set(
        "editor_insert",
        lua.create_function(move |_, text: String| {
            s.apply_editor_op(EditorSyncOp::Insert(text)).map_err(mlua::Error::runtime)?;
            Ok(())
        })?,
    )?;
    // editor_replace(range_or_nil, text)
    let s = sh!();
    ui.set(
        "editor_replace",
        lua.create_function(move |_, (range, text): (Value, String)| {
            let (start, end) = match range {
                Value::Nil => (None, None),
                Value::Table(t) => {
                    let st: Value = t.get("start")?;
                    let en: Value = t.get("end")?;
                    let (st, en) =
                        if st.is_nil() && en.is_nil() { (t.get::<Value>(1)?, t.get::<Value>(2)?) } else { (st, en) };
                    (Some(parse_pos(&st, "mare.ui.editor_replace")?), Some(parse_pos(&en, "mare.ui.editor_replace")?))
                }
                _ => {
                    return Err(mlua::Error::runtime(
                        "mare.ui.editor_replace: range must be nil or { start = {row, col}, ['end'] = {row, col} }",
                    ));
                }
            };
            s.apply_editor_op(EditorSyncOp::Replace { start, end, text }).map_err(mlua::Error::runtime)?;
            Ok(())
        })?,
    )?;
    let s = sh!();
    // flag(name, value): frontend display switches (show_reasoning, expand_all)
    ui.set(
        "flag",
        lua.create_function(move |_, (name, value): (String, bool)| {
            s.send(UiCmd::Flag { name, value });
            Ok(())
        })?,
    )?;
    let s = sh!();
    ui.set(
        "editor_hint",
        lua.create_function(move |_, text: String| {
            s.send(UiCmd::EditorHint { text });
            Ok(())
        })?,
    )?;
    // editor_config{prompt=, placeholder=, busy_placeholder=, border = "MareBorder",
    //   shell = {placeholder=, private_placeholder=, border=} | false}
    let s = sh!();
    ui.set(
        "editor_config",
        lua.create_function(move |_, t: Table| {
            s.send(UiCmd::EditorConfig {
                prompt: t.get("prompt")?,
                placeholder: t.get("placeholder")?,
                busy_placeholder: t.get("busy_placeholder")?,
                border: t.get("border")?,
                shell: match t.get::<Value>("shell")? {
                    Value::Nil => None,
                    Value::Table(st) => Some(Some(mare_shared::ui::ShellStyle {
                        placeholder: st.get::<Option<String>>("placeholder")?.unwrap_or_default(),
                        private_placeholder: st.get::<Option<String>>("private_placeholder")?.unwrap_or_default(),
                        border: st.get("border")?,
                    })),
                    _ => Some(None),
                },
            });
            Ok(())
        })?,
    )?;
    // style(name, spec) — set one named style (Neovim nvim_set_hl); style(name) reads it
    let s = sh!();
    ui.set(
        "style",
        lua.create_function(move |lua, (name, spec): (String, Option<Value>)| match spec {
            Some(v) => {
                let mut th = s.theme.write().unwrap();
                let st = style_from(&th, &v).resolve(&th);
                th.set_style(&name, st);
                drop(th);
                mare_shared::style::theme_changed();
                s.send(UiCmd::ThemeChanged);
                Ok(Value::Nil)
            }
            None => {
                let st = s.theme.read().unwrap().style(&name);
                let t = lua.create_table()?;
                let hex = |c: Color| match c {
                    Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                    Color::Ansi(n) => n.to_string(),
                    Color::Reset => "reset".into(),
                    Color::Theme(id) => mare_shared::style::theme_name(id).unwrap_or_default(),
                };
                t.set("fg", st.fg.map(hex))?;
                t.set("bg", st.bg.map(hex))?;
                t.set("bold", st.bold)?;
                t.set("italic", st.italic)?;
                t.set("dim", st.dim)?;
                t.set("underline", st.underline)?;
                t.set("strike", st.strike)?;
                Ok(Value::Table(t))
            }
        })?,
    )?;
    let s = sh!();
    ui.set("styles", lua.create_function(move |_, ()| Ok(s.theme.read().unwrap().names()))?)?;
    let s = sh!();
    ui.set("normalize_key", lua.create_function(move |_, k: String| Ok(normalize_seq_with(&s, &k)))?)?;
    let km_core = mare_shared::lua::core::keymap::handle(lua)?;
    // Shared validation helper for both `check` and `prepare_batch`: validates reserved keys,
    // bindable context/sequence/core-action target, `when` predicates, and (when requested)
    // default-layer printable-key guards.
    fn validate_binding_spec(
        km: &mare_shared::bindings::Keymap,
        s: &UiShared,
        context: &str,
        norm_key: &str,
        when: &Value,
        action: Option<&str>,
        check_printable: bool,
    ) -> Result<When, String> {
        for token in norm_key.split_whitespace() {
            let normalized = s.normalize_key(token);
            if mare_shared::bindings::is_reserved(&normalized) {
                return Err(format!(
                    "{} is reserved by the core and cannot be bound or unbound",
                    mare_shared::bindings::key_label(&normalized)
                ));
            }
        }
        km.check_binding_target(context, norm_key, action)?;
        let names: Vec<String> = match when {
            Value::Nil => vec![],
            Value::String(st) => vec![st.to_string_lossy()],
            Value::Table(t) => t.sequence_values::<String>().collect::<mlua::Result<_>>().map_err(|e| e.to_string())?,
            _ => return Err("`when` is a predicate name or a list of them".into()),
        };
        let parsed = When::parse(names.iter().map(String::as_str))?;
        if check_printable && parsed.all == 0 && parsed.none == 0 {
            let first_key = norm_key.split_whitespace().next().unwrap_or(norm_key);
            if mare_shared::bindings::is_printable_key_name(first_key)
                && let Some(def) = km.context_def(context)
                && !def.swallow_printable
                && km.chain_for(context).is_ok_and(|c| c.contains(&"text"))
            {
                return Err(format!(
                    "default-layer binding for printable key \"{norm_key}\" in \"{context}\" requires a `when` guard"
                ));
            }
        }
        Ok(parsed)
    }

    // check(context, key, when, action?) -> all, none | nil, error — validates a binding
    // before mare.keymap.set stacks it: bindable context, allowed sequences, core action support,
    // known predicates.
    let s = sh!();
    km_core.set(
        "check",
        lua.create_function(move |_, (context, key, when, action): (String, String, Value, Option<String>)| {
            let km = s.keymap.read().unwrap();
            Ok(match validate_binding_spec(&km, &s, &context, &key, &when, action.as_deref(), false) {
                Ok(w) => (Some(w.all), Some(w.none), None),
                Err(e) => (None, None, Some(e)),
            })
        })?,
    )?;
    // keymap context registration: registers a plugin subcontext (`parent` string, with
    // optional `{ swallow_printable = bool, active = bool }`), removes it and its descendants
    // (`parent` nil), or queries/toggles activation when `parent` is a table `{ active? = bool }`.
    let s = sh!();
    km_core.set(
        "context",
        lua.create_function(move |lua, (name, parent_or_opts, opts): (String, Value, Option<Table>)| {
            match parent_or_opts {
                Value::String(p) => {
                    let p = p.to_str()?.to_string();
                    let swallow = opts
                        .as_ref()
                        .and_then(|o| o.get::<Option<bool>>("swallow_printable").ok().flatten())
                        .unwrap_or(false);
                    let active =
                        opts.as_ref().and_then(|o| o.get::<Option<bool>>("active").ok().flatten()).unwrap_or(false);
                    s.keymap
                        .write()
                        .unwrap()
                        .register_context_with(name.clone(), p.clone(), swallow, active)
                        .map_err(mlua::Error::runtime)?;
                    s.send(UiCmd::SetContext { name, parent: Some(p), swallow_printable: swallow, active });
                    Ok(Value::Nil)
                }
                Value::Table(t) => match t.get::<Option<bool>>("active")? {
                    Some(on) => {
                        s.keymap.write().unwrap().set_context_active(&name, on).map_err(mlua::Error::runtime)?;
                        s.send(UiCmd::SetContextActive { name, active: on });
                        Ok(Value::Boolean(on))
                    }
                    None => Ok(Value::Boolean(s.keymap.read().unwrap().is_context_active(&name))),
                },
                Value::Nil => {
                    let removed = s.keymap.write().unwrap().remove_context(&name);
                    if !removed.is_empty() {
                        s.send(UiCmd::SetContext { name, parent: None, swallow_printable: false, active: false });
                    }
                    lua.to_value(&removed)
                }
                _ => Err(mlua::Error::runtime("mare.keymap.context: invalid arguments")),
            }
        })?,
    )?;
    // Every currently bindable context name.
    let s = sh!();
    km_core.set(
        "contexts",
        lua.create_function(move |_, ()| {
            Ok(s.keymap.read().unwrap().bindable_contexts().into_iter().map(str::to_owned).collect::<Vec<_>>())
        })?,
    )?;
    // Core action registry rows `{ name, desc, contexts, essential, declines }`.
    km_core.set(
        "core_actions",
        lua.create_function(move |lua, ()| {
            let array_mt = lua.array_metatable();
            let out = lua.create_table_with_capacity(CORE_ACTIONS.len(), 0)?;
            for (i, a) in CORE_ACTIONS.iter().enumerate() {
                let row = lua.create_table_with_capacity(0, 5)?;
                row.raw_set("name", a.name)?;
                row.raw_set("desc", a.desc)?;
                let ctxs = lua.create_table_with_capacity(a.contexts.len(), 0)?;
                for (j, c) in a.contexts.iter().enumerate() {
                    ctxs.raw_set(j + 1, *c)?;
                }
                if a.contexts.is_empty() {
                    ctxs.set_metatable(Some(array_mt.clone()))?;
                }
                row.raw_set("contexts", ctxs)?;
                row.raw_set("essential", a.essential)?;
                row.raw_set("declines", a.declines)?;
                out.raw_set(i + 1, row)?;
            }
            Ok(out)
        })?,
    )?;
    // prepare_batch(items) -> prepared | nil, err — normalizes keys and validates every
    // `{ context, key, when?, action?, check_printable? }` item in one keymap read lock.
    let s = sh!();
    km_core.set(
        "prepare_batch",
        lua.create_function(move |lua, items: Table| {
            let km = s.keymap.read().unwrap();
            let out = lua.create_table_with_capacity(items.raw_len(), 0)?;
            for (idx, item) in items.sequence_values::<Table>().enumerate() {
                let item = item?;
                let context: String = item.get("context")?;
                let raw_key: String = item.get("key")?;
                let when: Value = item.get("when")?;
                let action: Option<String> = item.get("action")?;
                let check_printable: bool = item.get::<Option<bool>>("check_printable")?.unwrap_or(false);
                let norm = normalize_seq_with(&s, &raw_key);
                let parsed =
                    match validate_binding_spec(&km, &s, &context, &norm, &when, action.as_deref(), check_printable) {
                        Ok(w) => w,
                        Err(e) => return Ok((Value::Nil, Some(e))),
                    };
                let row = lua.create_table_with_capacity(0, 4)?;
                row.set("key", norm)?;
                row.set("all", parsed.all)?;
                row.set("none", parsed.none)?;
                out.set(idx + 1, row)?;
            }
            Ok((Value::Table(out), None))
        })?,
    )?;
    // preset/keymap.lua sends a key's whole stack in one context after every change.
    fn parse_binding_entries(list: &Table) -> mlua::Result<Vec<Entry>> {
        let mut entries = Vec::with_capacity(list.raw_len());
        for e in list.sequence_values::<Table>() {
            let e = e?;
            let when =
                When { all: e.get::<Option<u8>>("all")?.unwrap_or(0), none: e.get::<Option<u8>>("none")?.unwrap_or(0) };
            let priority = mare_shared::bindings::Priority::new(e.get::<Option<f64>>("priority")?.unwrap_or(0.0))
                .map_err(mlua::Error::runtime)?;
            let action: String = e.get("action")?;
            let declines = e.get::<Option<bool>>("lua")?.unwrap_or(false)
                || mare_shared::bindings::find_core_action(&action).is_some_and(|c| c.declines);
            entries.push(Entry {
                action,
                unbound: e.get::<Option<bool>>("unbound")?.unwrap_or(false),
                when,
                declines,
                rank: e.get::<Option<u8>>("rank")?.unwrap_or(1),
                priority,
                seq: e.get::<Option<u64>>("seq")?.unwrap_or(0),
            });
        }
        Ok(entries)
    }
    let s = sh!();
    km_core.set(
        "set_bindings",
        lua.create_function(move |_, (context, key, list): (String, String, Table)| {
            s.keymap.read().unwrap().check_bindable(&context).map_err(mlua::Error::runtime)?;
            let entries = parse_binding_entries(&list)?;
            if s.keymap.write().unwrap().set(&context, key.clone(), entries.clone()) {
                s.send(UiCmd::SetBindings { context, key, entries });
            }
            Ok(())
        })?,
    )?;
    // Batch version of `set_bindings`: validates and parses every `{ context, key, list }` row
    // before mutating the keymap, then applies all rows under one write lock and sends one
    // `UiCmd::SetBindingsBatch`.
    let s = sh!();
    km_core.set(
        "set_bindings_batch",
        lua.create_function(move |_, updates: Table| {
            let mut parsed = Vec::with_capacity(updates.raw_len());
            {
                let km = s.keymap.read().unwrap();
                for row in updates.sequence_values::<Table>() {
                    let row = row?;
                    let context: String = row.get(1)?;
                    let key: String = row.get(2)?;
                    let list: Table = row.get(3)?;
                    km.check_bindable(&context).map_err(mlua::Error::runtime)?;
                    let entries = parse_binding_entries(&list)?;
                    parsed.push((context, key, entries));
                }
            }
            let mut km = s.keymap.write().unwrap();
            let mut changed = Vec::new();
            for (context, key, entries) in parsed {
                if km.set(&context, key.clone(), entries.clone()) {
                    changed.push((context, key, entries));
                }
            }
            drop(km);
            if !changed.is_empty() {
                s.send(UiCmd::SetBindingsBatch { updates: changed });
            }
            Ok(())
        })?,
    )?;
    // Context info `{ chain, swallow_printable, active } | nil`.
    let s = sh!();
    km_core.set(
        "context_info",
        lua.create_function(move |lua, context: String| {
            let km = s.keymap.read().unwrap();
            let Some(def) = km.context_def(&context) else {
                return Ok(Value::Nil);
            };
            let chain = km.chain_for(&context).map_err(mlua::Error::runtime)?;
            let row = lua.create_table()?;
            row.set("chain", chain)?;
            row.set("swallow_printable", def.swallow_printable)?;
            row.set("active", !context.contains('.') || km.is_context_active(&context))?;
            Ok(Value::Table(row))
        })?,
    )?;
    // Use the same lookup-chain and rank rules for Lua hints and the TUI.
    let s = sh!();
    km_core.set(
        "hint",
        lua.create_function(move |_, (action, context): (String, String)| {
            let keymap = s.keymap.read().unwrap();
            let chain = keymap.chain_for(&context).map_err(mlua::Error::runtime)?;
            Ok(keymap.hint(&action, &chain).into_iter().map(str::to_owned).collect::<Vec<_>>())
        })?,
    )?;
    // Shares `mare_shared::bindings::is_printable_key_name` with TUI dispatch.
    km_core.set(
        "is_printable",
        lua.create_function(move |_, key: String| Ok(mare_shared::bindings::is_printable_key_name(&key)))?,
    )?;
    km_core
        .set("is_reserved", lua.create_function(move |_, key: String| Ok(mare_shared::bindings::is_reserved(&key)))?)?;
    km_core.set(
        "is_core_action",
        lua.create_function(move |_, name: String| Ok(CORE_ACTIONS.iter().any(|a| a.name == name)))?,
    )?;
    let s = sh!();
    ui.set(
        "set_viewport",
        lua.create_function(move |_, (w, h): (Option<u16>, Option<u16>)| {
            use std::sync::atomic::Ordering;
            if w.is_some_and(|x| x == 0 || x > 10000) || h.is_some_and(|x| x == 0 || x > 10000) {
                return Err(mlua::Error::runtime("invalid viewport size (expected 1..=10000)"));
            }
            // An attached TUI owns viewport geometry; ignore Lua overrides that would diverge from App/Renderer.
            if s.editor_bridge.read().unwrap().is_some() {
                return Ok(false);
            }
            let mut changed = false;
            if let Some(w) = w
                && s.width.swap(w, Ordering::Relaxed) != w
            {
                changed = true;
            }
            if let Some(h) = h
                && s.height.swap(h, Ordering::Relaxed) != h
            {
                changed = true;
            }
            Ok(changed)
        })?,
    )?;
    let s = sh!();
    km_core.set(
        "resolve",
        lua.create_function(move |lua, (context, key, opts): (String, String, Option<Table>)| {
            let norm_key = normalize_seq_with(&s, &key);
            let mut bits = 0u8;
            if let Some(o) = opts {
                if o.get::<Option<bool>>("editor_empty")?.unwrap_or(false) {
                    bits |= mare_shared::bindings::EDITOR_EMPTY;
                }
                if o.get::<Option<bool>>("working")?.unwrap_or(false) {
                    bits |= mare_shared::bindings::WORKING;
                }
                if o.get::<Option<bool>>("completion_open")?.unwrap_or(false) {
                    bits |= mare_shared::bindings::COMPLETION_OPEN;
                }
                if o.get::<Option<bool>>("paste_burst")?.unwrap_or(false) {
                    bits |= mare_shared::bindings::PASTE_BURST;
                }
            }
            let km = s.keymap.read().unwrap();
            let chain = km.chain_for(&context).map_err(mlua::Error::runtime)?;
            let printable = mare_shared::bindings::is_printable_key_name(&norm_key);
            let (raw, masked) = km.chain_resolve(&chain, &norm_key, bits, printable);
            let candidates = lua.create_table()?;
            for (i, (c, a)) in raw.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("context", c)?;
                row.set("action", a)?;
                candidates.set(i + 1, row)?;
            }
            let out = lua.create_table()?;
            out.set("key", norm_key)?;
            out.set("masked", masked)?;
            out.set("candidates", candidates)?;
            Ok(out)
        })?,
    )?;
    let s = sh!();
    km_core.set(
        "snapshot",
        lua.create_function(move |lua, filter_context: Option<String>| {
            fn str_array(lua: &Lua, array_mt: &Table, items: &[&str]) -> mlua::Result<Table> {
                let t = lua.create_table_with_capacity(items.len(), 0)?;
                for (i, item) in items.iter().enumerate() {
                    t.raw_set(i + 1, *item)?;
                }
                if items.is_empty() {
                    t.set_metatable(Some(array_mt.clone()))?;
                }
                Ok(t)
            }
            // Fetch Lua-side actions and maps before acquiring the keymap read lock so we never
            // re-enter Lua while holding the lock and never have to clone the entire Keymap.
            let mare: Table = lua.globals().get("mare")?;
            let km_tbl: Table = mare.get("keymap")?;
            let actions_fn: mlua::Function = km_tbl.get("actions")?;
            let lua_actions: Table = actions_fn.call(())?;
            let maps: Table = mare_shared::lua::core::keymap::maps(lua)?
                .ok_or_else(|| mlua::Error::runtime("keymap maps table unavailable"))?;
            let array_mt = lua.array_metatable();

            let km = s.keymap.read().unwrap();
            let filter_chain: Option<Vec<&str>> = if let Some(ref ctx) = filter_context {
                Some(km.chain_for(ctx).map_err(mlua::Error::runtime)?)
            } else {
                None
            };
            let mut ctx_names: Vec<&str> = filter_chain.clone().unwrap_or_else(|| km.bindable_contexts());
            ctx_names.sort_unstable();
            ctx_names.dedup();
            let contexts = lua.create_table_with_capacity(ctx_names.len(), 0)?;
            let mut ctx_idx = 0usize;
            for name in &ctx_names {
                let Some(def) = km.context_def(name) else { continue };
                let Ok(chain) = km.chain_for(name) else { continue };
                ctx_idx += 1;
                let row = lua.create_table_with_capacity(0, 5)?;
                row.raw_set("name", *name)?;
                let parents = lua.create_table_with_capacity(def.parents.len(), 0)?;
                for (j, p) in def.parents.iter().enumerate() {
                    parents.raw_set(j + 1, p.as_str())?;
                }
                if def.parents.is_empty() {
                    parents.set_metatable(Some(array_mt.clone()))?;
                }
                row.raw_set("parents", parents)?;
                row.raw_set("core", def.core)?;
                row.raw_set("bindable", def.bindable)?;
                row.raw_set("chain", str_array(lua, &array_mt, &chain)?)?;
                contexts.raw_set(ctx_idx, row)?;
            }
            if ctx_idx == 0 {
                contexts.set_metatable(Some(array_mt.clone()))?;
            }

            let default_ctx = filter_context.as_deref().unwrap_or("editor");
            let hint_chain = km.chain_for(default_ctx).unwrap_or_else(|_| vec!["editor", "global"]);
            let hints_by_action = km.hints_for_chain(&hint_chain);
            let actions = lua.create_table_with_capacity(lua_actions.raw_len(), 0)?;
            let mut act_idx = 0usize;
            for item in lua_actions.sequence_values::<Table>() {
                let row = item?;
                let name = row.raw_get::<Option<mlua::LuaString>>("name")?;
                let Some(name_s) = name else { continue };
                let name_str = name_s.to_str()?;
                if name_str.is_empty() {
                    continue;
                }
                let keys = hints_by_action.get(name_str.as_ref()).map(Vec::as_slice).unwrap_or(&[]);
                row.raw_set("keys", str_array(lua, &array_mt, keys)?)?;
                row.raw_set("declines", Value::Nil)?;
                if let Ok(Value::Table(ctxs)) = row.raw_get::<Value>("contexts")
                    && ctxs.is_empty()
                    && ctxs.metatable().is_none()
                {
                    ctxs.set_metatable(Some(array_mt.clone()))?;
                }
                act_idx += 1;
                actions.raw_set(act_idx, row)?;
            }
            if act_idx == 0 {
                actions.set_metatable(Some(array_mt.clone()))?;
            }

            let mut binding_rows: Vec<(String, String, Table)> = Vec::new();
            for pair in maps.pairs::<mlua::LuaString, Table>() {
                let (key_lua, stack) = pair?;
                let key_str = key_lua.to_str()?;
                let stack_len = stack.raw_len();
                if stack_len == 0 {
                    continue;
                }
                let mut seen_ctx: Vec<String> = Vec::new();
                let mut key_lbl: Option<String> = None;
                for i in (1..=stack_len).rev() {
                    let entry: Table = stack.raw_get(i)?;
                    let context_lua: mlua::LuaString = entry.raw_get("context")?;
                    let context = context_lua.to_str()?;
                    if let Some(ref chain) = filter_chain
                        && !chain.iter().any(|c| *c == context.as_ref())
                    {
                        continue;
                    }
                    let action_val: Value = entry.raw_get("action")?;
                    let unbound = matches!(action_val, Value::Boolean(false));
                    let top = if seen_ctx.iter().any(|c| c == context.as_ref()) {
                        false
                    } else {
                        seen_ctx.push(context.to_string());
                        true
                    };
                    let when_tbl = match entry.raw_get::<Value>("when")? {
                        Value::String(w) => {
                            let t = lua.create_table_with_capacity(1, 0)?;
                            t.raw_set(1, w)?;
                            t
                        }
                        Value::Table(t) => {
                            let len = t.raw_len();
                            let out_w = lua.create_table_with_capacity(len, 0)?;
                            for j in 1..=len {
                                out_w.raw_set(j, t.raw_get::<Value>(j)?)?;
                            }
                            if len == 0 {
                                out_w.set_metatable(Some(array_mt.clone()))?;
                            }
                            out_w
                        }
                        _ => {
                            let t = lua.create_table()?;
                            t.set_metatable(Some(array_mt.clone()))?;
                            t
                        }
                    };
                    let seq = entry.raw_get::<Option<u64>>("seq")?.unwrap_or(0);
                    let declines = if let Value::String(ref act_s) = action_val {
                        let act_str = act_s.to_str()?;
                        km.stack(&context, &key_str)
                            .iter()
                            .find(|e| e.action == *act_str && e.seq == seq)
                            .is_some_and(|e| e.declines)
                    } else {
                        false
                    };
                    let rank_num = entry.raw_get::<Option<u8>>("rank")?.unwrap_or(1);
                    let rank = match rank_num {
                        4 => "project",
                        3 => "user",
                        2 => "plugin",
                        _ => "default",
                    };
                    let lbl = key_lbl.get_or_insert_with(|| mare_shared::bindings::key_label(&key_str));
                    let row = lua.create_table_with_capacity(0, 10)?;
                    row.raw_set("context", context_lua)?;
                    row.raw_set("key", key_lua.clone())?;
                    row.raw_set("key_label", lbl.as_str())?;
                    row.raw_set("action", action_val)?;
                    row.raw_set("unbound", unbound)?;
                    row.raw_set("when", when_tbl)?;
                    row.raw_set("declines", declines)?;
                    row.raw_set("rank", rank)?;
                    row.raw_set("desc", entry.raw_get::<Option<mlua::LuaString>>("desc")?)?;
                    row.raw_set("active", top && !unbound)?;
                    binding_rows.push((context.to_string(), key_str.to_string(), row));
                }
            }
            binding_rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            let bindings = lua.create_table_with_capacity(binding_rows.len(), 0)?;
            if binding_rows.is_empty() {
                bindings.set_metatable(Some(array_mt.clone()))?;
            } else {
                for (i, (_, _, row)) in binding_rows.into_iter().enumerate() {
                    bindings.raw_set(i + 1, row)?;
                }
            }
            let missing_items = km.missing_essential();
            let missing = lua.create_table_with_capacity(missing_items.len(), 0)?;
            if missing_items.is_empty() {
                missing.set_metatable(Some(array_mt.clone()))?;
            } else {
                for (i, (action, context)) in missing_items.into_iter().enumerate() {
                    let row = lua.create_table_with_capacity(0, 2)?;
                    row.raw_set("action", action)?;
                    row.raw_set("context", context)?;
                    missing.raw_set(i + 1, row)?;
                }
            }
            let unbound_items = km.unbound_essential_actions();
            let unbound = lua.create_table_with_capacity(unbound_items.len(), 0)?;
            if unbound_items.is_empty() {
                unbound.set_metatable(Some(array_mt.clone()))?;
            } else {
                for (i, (context, action)) in unbound_items.into_iter().enumerate() {
                    let row = lua.create_table_with_capacity(0, 2)?;
                    row.raw_set("context", context)?;
                    row.raw_set("action", action)?;
                    unbound.raw_set(i + 1, row)?;
                }
            }
            let conflicts = lua.create_table_with_capacity(0, 2)?;
            conflicts.raw_set("missing_essential", missing)?;
            conflicts.raw_set("unbound_essential", unbound)?;
            let out = lua.create_table_with_capacity(0, 4)?;
            out.raw_set("contexts", contexts)?;
            out.raw_set("actions", actions)?;
            out.raw_set("bindings", bindings)?;
            out.raw_set("conflicts", conflicts)?;
            Ok(out)
        })?,
    )?;
    // keymap() -> { key = action } of the input (the effective binding of each key in editor)
    let s = sh!();
    ui.set(
        "keymap",
        lua.create_function(move |_, ()| {
            let keymap = s.keymap.read().unwrap();
            let chain = keymap.chain_for("editor").map_err(mlua::Error::runtime)?;
            Ok(keymap.bindings_in_chain(&chain).into_iter().collect::<HashMap<_, _>>())
        })?,
    )?;
    // theme({slot = "#rrggbb", ...}) — merge colors into the theme
    let s = sh!();
    ui.set(
        "theme",
        lua.create_function(move |_, t: Option<Table>| {
            if let Some(t) = t {
                let mut th = s.theme.write().unwrap();
                for p in t.pairs::<String, String>() {
                    let (k, v) = p?;
                    if let Some(c) = Color::parse(&v) {
                        th.set(&k, c);
                    }
                }
                drop(th);
                mare_shared::style::theme_changed();
                s.send(UiCmd::ThemeChanged);
            }
            Ok(())
        })?,
    )?;
    // Validate before publishing; malformed syntax documents leave the active theme intact.
    let s = sh!();
    ui.set(
        "syntax_theme",
        lua.create_function(move |_, (document, validate_only): (Option<String>, Option<bool>)| {
            if let Some(ref text) = document {
                if text.len() > 1024 * 1024 {
                    return Err(mlua::Error::runtime("syntax theme exceeds 1 MiB"));
                }
                syntect::highlighting::ThemeSet::load_from_reader(&mut std::io::Cursor::new(text.as_bytes()))
                    .map_err(mlua::Error::external)?;
            }
            if validate_only.unwrap_or(false) {
                return Ok(());
            }
            s.theme.write().unwrap().syntax = document;
            mare_shared::style::theme_changed();
            s.send(UiCmd::ThemeChanged);
            Ok(())
        })?,
    )?;
    Ok(())
}
