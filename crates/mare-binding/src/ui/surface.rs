//! Transcript blocks, status/widgets, overlay/transcript views, notifications, and surface queries.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use mlua::{Lua, Table, Value};

use super::{BlockPatch, Node, UiCmd, UiShared, node_from, patch_from};

pub(super) fn install(lua: &Lua, ui: &Table, shared: &Arc<UiShared>) -> mlua::Result<()> {
    macro_rules! sh {
        () => {
            shared.clone()
        };
    }

    ui.set("image_id", lua.create_function(|_, ()| crate::ui_node::new_image_id())?)?;
    ui.set("layout_revision", lua.create_function(|_, ()| Ok(mare_shared::style::theme_gen()))?)?;
    ui.set(
        "markdown_blocks",
        lua.create_function(|lua, text: String| {
            let out = lua.create_table()?;
            for (index, block) in mare_shared::markdown::fences(&text).into_iter().enumerate() {
                let value = lua.create_table()?;
                value.set("start", block.start)?;
                value.set("end", block.end)?;
                value.set("source", block.source)?;
                value.set("language", block.language)?;
                value.set("info", block.info)?;
                value.set("closed", block.closed)?;
                out.set(index + 1, value)?;
            }
            Ok(out)
        })?,
    )?;
    let s = sh!();
    ui.set(
        "validate_node",
        lua.create_function(move |lua, value: Value| {
            let node = node_from(&s.theme.read().unwrap(), value)?;
            let info = lua.create_table()?;
            info.set("images", node.contains_images())?;
            Ok((!matches!(node, Node::Empty), info))
        })?,
    )?;
    let s = sh!();
    ui.set("connected", lua.create_function(move |_, ()| Ok(s.connected()))?)?;
    let s = sh!();
    ui.set("can_respond", lua.create_function(move |_, ()| Ok(s.can_respond()))?)?;
    let s = sh!();
    ui.set("width", lua.create_function(move |_, ()| Ok(s.width.load(Ordering::Relaxed)))?)?;
    // display_path(path, max_width?) -> "~/…" with middle elision (mare_shared::style::display_path)
    ui.set(
        "display_path",
        lua.create_function(|_, (path, max): (String, Option<usize>)| {
            let home = dirs::home_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            Ok(mare_shared::style::display_path(&path, &home, max.unwrap_or(0)))
        })?,
    )?;
    let s = sh!();
    ui.set("height", lua.create_function(move |_, ()| Ok(s.height.load(Ordering::Relaxed)))?)?;

    // push(kind, {node=, markdown=, expanded=}) -> id
    let s = sh!();
    ui.set(
        "push",
        lua.create_function(move |lua, (kind, t): (String, Option<Table>)| {
            let id = s.next_id.fetch_add(1, Ordering::Relaxed);
            let patch = match t {
                Some(t) => patch_from(lua, &s, &t)?,
                None => BlockPatch::default(),
            };
            s.send(UiCmd::BlockPush { id, kind, patch });
            Ok(id)
        })?,
    )?;
    let s = sh!();
    ui.set(
        "update",
        lua.create_function(move |lua, (id, t): (u64, Table)| {
            let patch = patch_from(lua, &s, &t)?;
            s.send(UiCmd::BlockUpdate { id, patch });
            Ok(())
        })?,
    )?;
    // append(id, text): streaming markdown append (fast path)
    let s = sh!();
    ui.set(
        "append",
        lua.create_function(move |_, (id, text): (u64, String)| {
            s.send(UiCmd::BlockUpdate { id, patch: BlockPatch { append: Some(text), ..Default::default() } });
            Ok(())
        })?,
    )?;
    let s = sh!();
    ui.set(
        "finish",
        lua.create_function(move |_, id: u64| {
            s.send(UiCmd::BlockFinish { id });
            Ok(())
        })?,
    )?;
    let s = sh!();
    ui.set(
        "remove",
        lua.create_function(move |_, id: u64| {
            s.send(UiCmd::BlockRemove { id });
            Ok(())
        })?,
    )?;
    // print(node): write directly into scrollback (above the active area)
    let s = sh!();
    ui.set(
        "print",
        lua.create_function(move |_, v: Value| {
            let theme = s.theme.read().unwrap().clone();
            let node = node_from(&theme, v)?;
            if !s.send(UiCmd::Print { node: node.clone() }) {
                // headless: plain text to stderr
                let _ = &theme;
                let lines = s.render(&node, 100);
                for l in lines {
                    eprintln!("{}", l.plain());
                }
            }
            Ok(())
        })?,
    )?;
    // set_widget(id, slot, node|nil, order)
    let s = sh!();
    ui.set(
        "set_widget",
        lua.create_function(move |_, (id, slot, v, order): (String, Option<String>, Value, Option<i64>)| {
            let slot = match slot.as_deref() {
                Some(name) => mare_shared::layout::validate_widget_slot(name).map_err(mlua::Error::RuntimeError)?,
                None => "above-editor",
            };
            let theme = s.theme.read().unwrap().clone();
            let node = match v {
                Value::Nil => None,
                v => Some(node_from(&theme, v)?),
            };
            s.send(UiCmd::Widget { id, slot: slot.into(), order: order.unwrap_or(0), node });
            Ok(())
        })?,
    )?;
    // set_status(id, align, node|nil, order, keep) — `keep`: stays whole on a narrow terminal
    let s = sh!();
    ui.set(
        "set_status",
        lua.create_function(
            move |_, (id, align, v, order, keep): (String, Option<String>, Value, Option<i64>, Option<bool>)| {
                let align = match align.as_deref() {
                    Some(name) => {
                        mare_shared::layout::validate_status_align(name).map_err(mlua::Error::RuntimeError)?
                    }
                    None => "left",
                };
                let theme = s.theme.read().unwrap().clone();
                let node = match v {
                    Value::Nil => None,
                    v => Some(node_from(&theme, v)?),
                };
                s.send(UiCmd::Status {
                    id,
                    align: align.into(),
                    order: order.unwrap_or(0),
                    node,
                    keep: keep.unwrap_or(false),
                });
                Ok(())
            },
        )?,
    )?;
    // set_layout(layout|nil, opts?) — validates, persists to `mare.o.ui` unless `opts.persist == false`, and applies
    let s = sh!();
    ui.set(
        "set_layout",
        lua.create_function(move |lua, (spec, opts): (Value, Option<Table>)| {
            let persist = match opts {
                Some(ref t) => match t.get::<Value>("persist")? {
                    Value::Nil => true,
                    Value::Boolean(b) => b,
                    _ => return Err(mlua::Error::runtime("ui.set_layout: opts.persist must be a boolean")),
                },
                None => true,
            };
            let cfg = parse_layout_config(spec.clone(), None)?;
            if persist
                && let Ok(mare_tbl) = lua.globals().get::<Table>("mare")
                && let Ok(Value::Table(o)) = mare_tbl.get::<Value>("o")
            {
                let ui_tbl = lua.create_table()?;
                let layout_tbl = normalize_layout_for_option(lua, spec)?;
                ui_tbl.set("layout", layout_tbl)?;
                o.set("ui", ui_tbl)?;
            }
            *s.layout.write().unwrap() = cfg.clone();
            s.send(UiCmd::Layout(cfg));
            Ok(())
        })?,
    )?;
    // working(label|nil, detail, {background = true}) — `background`: a shell command the
    // background key can move aside
    let s = sh!();
    ui.set(
        "working",
        lua.create_function(move |_, (label, detail, opts): (Option<String>, Option<String>, Option<Table>)| {
            let background = opts.and_then(|o| o.get::<Option<bool>>("background").ok().flatten()).unwrap_or(false);
            s.send(UiCmd::Working { label, detail, background });
            Ok(())
        })?,
    )?;
    // notify_os(text, {only_unfocused = false, protocol = "auto"|"osc9"|"osc777"|"osc99"|"bel"|"native",
    //   bell = false}) -> sent? `only_unfocused` holds back only while the terminal is known to be
    // focused: a terminal that never reported focus counts as unknown, and the notice goes out
    let s = sh!();
    ui.set(
        "notify_os",
        lua.create_function(move |_, (text, opts): (String, Option<Table>)| {
            let get_b = |k: &str| opts.as_ref().and_then(|o| o.get::<Option<bool>>(k).ok().flatten());
            let only_unfocused = get_b("only_unfocused").unwrap_or(false);
            if only_unfocused && s.focus_known.load(Ordering::Relaxed) && s.focused.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let protocol = opts
                .as_ref()
                .and_then(|o| o.get::<Option<String>>("protocol").ok().flatten())
                .unwrap_or_else(|| "auto".into());
            let bell = get_b("bell").unwrap_or(false);
            Ok(s.send(UiCmd::NotifyOs { text, protocol, bell }))
        })?,
    )?;
    let s = sh!();
    // set_queued(n): how many messages wait in the agent queue (drives ↑ / Enter on empty input)
    ui.set(
        "set_queued",
        lua.create_function(move |_, n: usize| {
            if s.queued.swap(n, Ordering::Relaxed) != n {
                s.send(UiCmd::Redraw);
            }
            Ok(())
        })?,
    )?;
    let s = sh!();
    // set_queue_mode(n): 0 nothing selected, 1 a queued message selected, 2 one being edited
    ui.set(
        "set_queue_mode",
        lua.create_function(move |_, n: u8| {
            if s.queue_mode.swap(n, Ordering::Relaxed) != n {
                s.send(UiCmd::Redraw);
            }
            Ok(())
        })?,
    )?;
    let s = sh!();
    // focused() -> true / false, or nil while the terminal hasn't reported focus yet
    ui.set(
        "focused",
        lua.create_function(move |_, ()| {
            Ok(s.focus_known.load(Ordering::Relaxed).then(|| s.focused.load(Ordering::Relaxed)))
        })?,
    )?;
    // caps() -> the terminal capability table (static table + startup probe + `ui/caps`
    // listeners): {sync, scroll_region, truecolor, grapheme, kitty_keyboard, position_known, …}
    let s2 = shared.clone();
    ui.set("caps", lua.create_function(move |lua, ()| mare_shared::lua::json_to_lua(lua, &s2.caps.read().unwrap()))?)?;
    // slow_frames() -> { {at_ms, ms, what}, … } frames over 16ms, newest last (128 kept)
    ui.set(
        "slow_frames",
        lua.create_function(|lua, ()| {
            let v: Vec<serde_json::Value> = mare_shared::profile::slow_frames()
                .into_iter()
                .map(|(at, ms, what)| serde_json::json!({ "at_ms": at, "ms": ms, "what": what }))
                .collect();
            mare_shared::lua::json_to_lua(lua, &serde_json::Value::Array(v))
        })?,
    )?;
    let s = sh!();
    ui.set(
        "notification_history",
        lua.create_function(move |lua, ()| {
            let out = lua.create_table()?;
            for (index, (text, level)) in s.notification_history().into_iter().enumerate() {
                let item = lua.create_table()?;
                item.set("text", text)?;
                item.set("level", level)?;
                out.set(index + 1, item)?;
            }
            Ok(out)
        })?,
    )?;
    let s = sh!();
    ui.set(
        "notify",
        lua.create_function(move |_, (text, level, opts): (String, Option<String>, Option<mlua::Table>)| {
            let level = level.unwrap_or_else(|| "info".into());
            let id = opts.map(|table| table.get::<Option<String>>("id")).transpose()?.flatten();
            if !s.send(UiCmd::Notify { text: text.clone(), level: level.clone(), id }) {
                eprintln!("[{level}] {text}");
            }
            Ok(())
        })?,
    )?;
    // overlay(title, node, opts?): read-only or focused custom popup (Esc closes)
    let s = sh!();
    ui.set(
        "overlay",
        lua.create_function(move |_, (title, v, opts): (String, Value, Option<Table>)| {
            let OverlayOpts { context, cursor } = parse_overlay_opts(&s.keymap.read().unwrap(), opts)?;
            let theme = s.theme.read().unwrap().clone();
            s.send(UiCmd::Overlay { title, node: node_from(&theme, v)?, context, cursor });
            Ok(())
        })?,
    )?;
    // transcript(title, node, opts?): full-screen transcript view (Ctrl+T style) with this
    // content; calling again with the same title while it's open refreshes it in place (live
    // tailing). opts.keys = { { action = "a" | {"a", "b"}, label = "…" }, … }: the view's own keys,
    // shown in its header by their current bindings (transcript layer)
    let s = sh!();
    ui.set(
        "transcript",
        lua.create_function(move |_, (title, v, opts): (String, Value, Option<Table>)| {
            let theme = s.theme.read().unwrap().clone();
            let mut keys = vec![];
            if let Some(list) = opts.map(|o| o.get::<Option<Table>>("keys")).transpose()?.flatten() {
                for e in list.sequence_values::<Table>() {
                    let e = e?;
                    let actions = match e.get::<Value>("action")? {
                        Value::String(a) => vec![a.to_str()?.to_string()],
                        Value::Table(t) => t.sequence_values::<String>().collect::<mlua::Result<_>>()?,
                        _ => {
                            return Err(mlua::Error::runtime(
                                "ui.transcript: keys[i].action must be a string or a list",
                            ));
                        }
                    };
                    keys.push(mare_shared::ui::ViewKey {
                        actions,
                        label: e.get::<Option<String>>("label")?.unwrap_or_default(),
                    });
                }
            }
            s.send(UiCmd::TranscriptView { title, node: node_from(&theme, v)?, keys });
            Ok(())
        })?,
    )?;
    // transcript_title() -> title of the plugin transcript view while open, else nil
    let s = sh!();
    ui.set("transcript_title", lua.create_function(move |_, ()| Ok(s.transcript_title.read().unwrap().clone()))?)?;
    // transcript_close(title?): close the plugin transcript view (only while it shows `title`)
    let s = sh!();
    ui.set(
        "transcript_close",
        lua.create_function(move |_, title: Option<String>| {
            s.send(UiCmd::TranscriptClose { title });
            Ok(())
        })?,
    )?;
    let s = sh!();
    ui.set(
        "clear",
        lua.create_function(move |_, ()| {
            s.send(UiCmd::Clear);
            Ok(())
        })?,
    )?;
    let s = sh!();
    // exit({force?}): leave; without force an exit guard (exit_guard) gets to ask first
    ui.set(
        "exit",
        lua.create_function(move |_, opts: Option<Table>| {
            let force = opts.and_then(|o| o.get::<Option<bool>>("force").ok().flatten()).unwrap_or(false);
            s.send(if force { UiCmd::ExitForce } else { UiCmd::Exit });
            Ok(())
        })?,
    )?;
    // exit_guard(on): route user-initiated exits through the `app/exit-requested` event
    let s = sh!();
    ui.set(
        "exit_guard",
        lua.create_function(move |_, on: bool| {
            s.send(UiCmd::ExitGuard { on });
            Ok(())
        })?,
    )?;
    // render(node, width) -> plain text lines (handy for headless/export)
    let s = sh!();
    ui.set(
        "render_text",
        lua.create_function(move |_, (v, w): (Value, Option<usize>)| {
            let theme = s.theme.read().unwrap().clone();
            let node = node_from(&theme, v)?;
            let _ = &theme;
            let lines = s.render(&node, w.unwrap_or(80));
            Ok(lines.iter().map(|l| l.plain()).collect::<Vec<_>>().join("\n"))
        })?,
    )?;
    Ok(())
}

fn parse_layout_config(spec: Value, extra_regions: Option<Table>) -> mlua::Result<mare_shared::layout::LayoutConfig> {
    use mare_shared::layout::{LayoutConfig, RawRegionConfig};
    use std::collections::BTreeMap;

    let mut order: Option<Vec<String>> = None;
    let mut raw_regions: BTreeMap<String, RawRegionConfig> = BTreeMap::new();

    let parse_regions_table = |reg_tbl: &Table, out: &mut BTreeMap<String, RawRegionConfig>| -> mlua::Result<()> {
        for pair in reg_tbl.pairs::<Value, Value>() {
            let (k, v) = pair?;
            let name = match k {
                Value::String(s) => s.to_str()?.to_string(),
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "ui.layout.regions key must be a string, got {}",
                        other.type_name()
                    )));
                }
            };
            if name.len() > mare_shared::layout::MAX_REGION_NAME_LEN {
                return Err(mlua::Error::RuntimeError(format!(
                    "unknown layout region (name too long: {} bytes, max {})",
                    name.len(),
                    mare_shared::layout::MAX_REGION_NAME_LEN
                )));
            }
            let entry_tbl = match v {
                Value::Table(t) => t,
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "ui.layout.regions.{name} must be a table, got {}",
                        other.type_name()
                    )));
                }
            };
            let mut raw = RawRegionConfig::default();
            for ep in entry_tbl.pairs::<Value, Value>() {
                let (ek, ev) = ep?;
                let key = match ek {
                    Value::String(s) => s.to_str()?.to_string(),
                    other => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "ui.layout.regions.{name} key must be a string, got {}",
                            other.type_name()
                        )));
                    }
                };
                match key.as_str() {
                    "max_height" => match ev {
                        Value::Integer(n) if n >= 0 => raw.max_height = Some(n as usize),
                        Value::Number(n) if n >= 0.0 && n.fract() == 0.0 => raw.max_height = Some(n as usize),
                        _ => {
                            return Err(mlua::Error::RuntimeError(format!(
                                "ui.layout.regions.{name}.max_height must be a positive integer"
                            )));
                        }
                    },
                    "hidden" => match ev {
                        Value::Boolean(b) => raw.hidden = Some(b),
                        _ => {
                            return Err(mlua::Error::RuntimeError(format!(
                                "ui.layout.regions.{name}.hidden must be a boolean"
                            )));
                        }
                    },
                    other => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "unknown key `{other}` in ui.layout.regions.{name}"
                        )));
                    }
                }
            }
            out.insert(name, raw);
        }
        Ok(())
    };

    if let Some(ref reg_tbl) = extra_regions {
        parse_regions_table(reg_tbl, &mut raw_regions)?;
    }

    match spec {
        Value::Nil => {}
        Value::Table(t) => {
            let len = t.raw_len();
            let mut array_items = Vec::new();
            if len > 0 {
                for i in 1..=len {
                    match t.raw_get::<Value>(i)? {
                        Value::String(s) => array_items.push(s.to_str()?.to_string()),
                        other => {
                            return Err(mlua::Error::RuntimeError(format!(
                                "ui.layout[{i}] must be a region name string, got {}",
                                other.type_name()
                            )));
                        }
                    }
                }
                order = Some(array_items);
            }
            for pair in t.pairs::<Value, Value>() {
                let (k, v) = pair?;
                match k {
                    Value::Integer(i) if i >= 1 && (i as usize) <= len => {}
                    Value::String(s) => {
                        let key = s.to_str()?.to_string();
                        match key.as_str() {
                            "order" => {
                                if order.is_some() {
                                    return Err(mlua::Error::RuntimeError(
                                        "ui.layout cannot specify both array items and `order`".into(),
                                    ));
                                }
                                let ot = match v {
                                    Value::Table(ot) => ot,
                                    other => {
                                        return Err(mlua::Error::RuntimeError(format!(
                                            "ui.layout.order must be a list of region names, got {}",
                                            other.type_name()
                                        )));
                                    }
                                };
                                let olen = ot.raw_len();
                                let mut items = Vec::with_capacity(olen);
                                for i in 1..=olen {
                                    match ot.raw_get::<Value>(i)? {
                                        Value::String(rs) => items.push(rs.to_str()?.to_string()),
                                        other => {
                                            return Err(mlua::Error::RuntimeError(format!(
                                                "ui.layout.order[{i}] must be a region name string, got {}",
                                                other.type_name()
                                            )));
                                        }
                                    }
                                }
                                order = Some(items);
                            }
                            "regions" => {
                                let rt = match v {
                                    Value::Table(rt) => rt,
                                    other => {
                                        return Err(mlua::Error::RuntimeError(format!(
                                            "ui.layout.regions must be a table, got {}",
                                            other.type_name()
                                        )));
                                    }
                                };
                                parse_regions_table(&rt, &mut raw_regions)?;
                            }
                            other => {
                                return Err(mlua::Error::RuntimeError(format!("unknown key `{other}` in ui.layout")));
                            }
                        }
                    }
                    other => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "unexpected key `{}` in ui.layout",
                            other.type_name()
                        )));
                    }
                }
            }
        }
        other => {
            return Err(mlua::Error::RuntimeError(format!(
                "ui.layout must be a table or nil, got {}",
                other.type_name()
            )));
        }
    }

    LayoutConfig::validate(order, raw_regions).map_err(mlua::Error::RuntimeError)
}

fn normalize_layout_for_option(lua: &Lua, spec: Value) -> mlua::Result<Table> {
    let out = lua.create_table()?;
    match spec {
        Value::Nil => {
            let order = lua.create_table()?;
            for (i, r) in mare_shared::layout::DEFAULT_STACK_ORDER.iter().enumerate() {
                order.set(i + 1, r.as_str())?;
            }
            out.set("order", order)?;
            out.set("regions", lua.create_table()?)?;
        }
        Value::Table(t) => {
            let len = t.raw_len();
            if len > 0 {
                let order = lua.create_table()?;
                for i in 1..=len {
                    order.set(i, t.raw_get::<Value>(i)?)?;
                }
                out.set("order", order)?;
            } else if let Ok(v) = t.get::<Value>("order")
                && !v.is_nil()
            {
                out.set("order", v)?;
            }
            if let Ok(v) = t.get::<Value>("regions")
                && !v.is_nil()
            {
                out.set("regions", v)?;
            } else {
                out.set("regions", lua.create_table()?)?;
            }
        }
        _ => {}
    }
    Ok(out)
}

fn parse_nonneg_int(tbl: &Table, key: &str) -> mlua::Result<usize> {
    match tbl.get::<Value>(key)? {
        Value::Integer(n) if n >= 0 => usize::try_from(n)
            .map_err(|_| mlua::Error::RuntimeError(format!("ui.overlay: cursor.{key} is out of range"))),
        _ => Err(mlua::Error::RuntimeError(format!("ui.overlay: cursor.{key} must be a non-negative integer"))),
    }
}

#[derive(Default)]
struct OverlayOpts {
    context: Option<String>,
    cursor: Option<(usize, usize)>,
}

fn parse_overlay_opts(keymap: &mare_shared::bindings::Keymap, opts: Option<Table>) -> mlua::Result<OverlayOpts> {
    let Some(opts) = opts else {
        return Ok(OverlayOpts::default());
    };
    let context = opts.get::<Option<String>>("context")?;
    if let Some(ref ctx) = context {
        let chain = keymap.chain_for(ctx).map_err(|e| mlua::Error::RuntimeError(format!("ui.overlay: {e}")))?;
        if !chain.contains(&"info") {
            return Err(mlua::Error::RuntimeError(format!(
                "ui.overlay: context \"{ctx}\" must be \"info\" or a registered \"info.*\" subcontext"
            )));
        }
        keymap.check_bindable(ctx).map_err(|e| mlua::Error::RuntimeError(format!("ui.overlay: {e}")))?;
    }
    let cursor = match opts.get::<Value>("cursor")? {
        Value::Nil => None,
        Value::Table(cur) => Some((parse_nonneg_int(&cur, "row")?, parse_nonneg_int(&cur, "col")?)),
        _ => {
            return Err(mlua::Error::RuntimeError("ui.overlay: opts.cursor must be a { row, col } table".into()));
        }
    };
    Ok(OverlayOpts { context, cursor })
}
