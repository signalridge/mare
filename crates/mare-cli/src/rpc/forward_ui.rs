//! `mare.ui.*` forwarded to the host while connected, restored on disconnect.

use mlua::LuaSerdeExt;
use std::cell::Cell;

use mare_binding::ui_wire::encode_node_or_string_json;

use super::*;

struct SelectGuard<'a> {
    conn: &'a Conn,
    host: &'a Host,
    select_id: Option<String>,
    open: Rc<Cell<bool>>,
}

impl Drop for SelectGuard<'_> {
    fn drop(&mut self) {
        self.open.set(false);
        if let Some(id) = self.select_id.take() {
            self.host.answerer.remove_active_select(self.conn.id, &id);
        }
    }
}

fn parse_cancel(t: &Table) -> mlua::Result<Option<tokio_util::sync::CancellationToken>> {
    let Some(ud) = t.get::<Option<mlua::AnyUserData>>("cancel")? else {
        return Ok(None);
    };
    Ok(Some(ud.borrow::<mare_binding::task::Token>()?.0.clone()))
}

fn ui_unavailable(conn: &Conn, host: &Host) -> bool {
    conn.closed.get()
        || (!conn.caps.borrow().ui && !host.shell.is_responder(conn.id))
        || !host.answerer.has_saved_ui(conn.id)
        || !access::authorize(conn, host, access::Op::Answerer).is_allowed()
}

async fn cancel_or_pending(cancel: Option<&tokio_util::sync::CancellationToken>) {
    match cancel {
        Some(c) => c.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

fn map_call_result(res: Result<Option<J>, ()>) -> Result<J, &'static str> {
    match res {
        Err(()) => Err("cancelled"),
        Ok(None) => Err("unavailable"),
        Ok(Some(v)) => Ok(v),
    }
}

async fn call_ui(
    conn: Option<&Rc<Conn>>,
    host: Option<&Arc<Host>>,
    session_id: &str,
    method: &str,
    params: J,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<J, &'static str> {
    let (Some(conn), Some(host)) = (conn, host) else {
        return Err("unavailable");
    };
    if ui_unavailable(conn, host) {
        return Err("unavailable");
    }
    let cap_rev = conn.cap_revision.get();
    let res = map_call_result(call_with_cancel(conn, session_id, method, params, cancel).await);
    if conn.cap_revision.get() != cap_rev || ui_unavailable(conn, host) {
        return Err("unavailable");
    }
    res
}

fn dismiss_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("interrupted") => "interrupted",
        _ => "cancelled",
    }
}

async fn rpc_text_prompt(
    conn: Option<Rc<Conn>>,
    host: Option<Arc<Host>>,
    session_id: &str,
    method: &str,
    params: J,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> (Option<String>, Option<String>) {
    match call_ui(conn.as_ref(), host.as_ref(), session_id, method, params, cancel.as_ref()).await {
        Err(reason) => (None, Some(reason.to_string())),
        Ok(v) => {
            let proto::UiTextPromptResult { text, reason } = serde_json::from_value(v).unwrap_or_default();
            match text {
                Some(text) => (Some(text), None),
                None => (None, Some(dismiss_reason(reason.as_deref()).to_string())),
            }
        }
    }
}

async fn call_with_cancel(
    conn: &Conn,
    session_id: &str,
    method: &str,
    params: J,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<Option<J>, ()> {
    if cancel.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        return Err(());
    }
    let fut = conn.call_with_timeout_for_session(method, session_id, params, None);
    tokio::select! {
        r = fut => Ok(r),
        _ = cancel_or_pending(cancel) => Err(()),
    }
}

fn parse_select_item(lua: &mlua::Lua, v: Value) -> mlua::Result<Option<proto::UiSelectItem>> {
    match v {
        Value::String(s) => Ok(Some(proto::UiSelectItem { label: s.to_str()?.to_string(), ..Default::default() })),
        Value::Table(it) => {
            let item_id = match it.get::<Value>("item_id")? {
                Value::String(s) => Some(s.to_str()?.to_string()),
                Value::Integer(n) => Some(n.to_string()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            };
            let preview = encode_node_or_string_json(lua, it.get::<Value>("preview")?)?;
            let mut lines = vec![];
            if let Some(ls) = it.get::<Option<Table>>("lines")? {
                for lv in ls.sequence_values::<Value>() {
                    if let Some(w) = encode_node_or_string_json(lua, lv?)? {
                        lines.push(w);
                    }
                }
            }
            Ok(Some(proto::UiSelectItem {
                item_id,
                label: it.get::<Option<String>>("label")?.unwrap_or_default(),
                detail: it.get("detail")?,
                preview,
                header: it.get("header")?,
                disabled: it.get("disabled")?,
                checked: it.get::<Option<bool>>("checked")?.unwrap_or(false),
                lines,
            }))
        }
        _ => Ok(None),
    }
}

fn parse_select_items(lua: &mlua::Lua, list: Option<Table>) -> mlua::Result<Vec<proto::UiSelectItem>> {
    let mut items = vec![];
    if let Some(list) = list {
        for v in list.sequence_values::<Value>() {
            if let Some(item) = parse_select_item(lua, v?)? {
                items.push(item);
            }
        }
    }
    Ok(items)
}

fn parse_select_tabs(lua: &mlua::Lua, list: Option<Table>) -> mlua::Result<Vec<proto::UiSelectTab>> {
    let mut tabs = vec![];
    if let Some(list) = list {
        for v in list.sequence_values::<Value>() {
            if let Value::Table(tt) = v? {
                tabs.push(proto::UiSelectTab {
                    name: tt.get::<Option<String>>("name")?.unwrap_or_default(),
                    items: parse_select_items(lua, tt.get::<Option<Table>>("items")?)?,
                    selected: tt.get::<Option<usize>>("selected")?.unwrap_or(1).saturating_sub(1),
                });
            }
        }
    }
    Ok(tabs)
}

fn make_select_handle(
    lua: &mlua::Lua,
    conn: &Rc<Conn>,
    host: &Arc<Host>,
    select_id: String,
    open: Rc<Cell<bool>>,
    close_notify: Rc<tokio::sync::Notify>,
) -> mlua::Result<Table> {
    let handle = lua.create_table()?;
    let open_chk = open.clone();
    handle.set("is_open", lua.create_function(move |_, _: Value| Ok(open_chk.get()))?)?;

    let w = Rc::downgrade(conn);
    let wh = Arc::downgrade(host);
    let sid = select_id.clone();
    let open_upd = open.clone();
    handle.set(
        "update",
        lua.create_function(move |lua, (_self, t): (Value, Table)| {
            if !open_upd.get() {
                return Ok(false);
            }
            let (Some(conn), Some(_host)) = (w.upgrade(), wh.upgrade()) else {
                open_upd.set(false);
                return Ok(false);
            };
            if conn.closed.get() {
                open_upd.set(false);
                return Ok(false);
            }
            let tabs = parse_select_tabs(lua, t.get::<Option<Table>>("tabs")?)?;
            let items = if tabs.is_empty() {
                let items_tbl = match t.get::<Option<Table>>("items")? {
                    Some(tbl) => Some(tbl),
                    None => Some(t),
                };
                parse_select_items(lua, items_tbl)?
            } else {
                vec![]
            };
            let params = proto::UiSelectUpdateParams { select_id: sid.clone(), items, tabs };
            conn.notify(methods::UI_SELECT_UPDATE, serde_json::to_value(params).unwrap_or(J::Null));
            Ok(true)
        })?,
    )?;

    let w = Rc::downgrade(conn);
    handle.set(
        "close",
        lua.create_function(move |_, _: Value| {
            if !open.get() {
                return Ok(false);
            }
            open.set(false);
            if let Some(conn) = w.upgrade()
                && !conn.closed.get()
            {
                let params = proto::UiSelectCloseParams { select_id: select_id.clone() };
                conn.notify(methods::UI_SELECT_CLOSE, serde_json::to_value(params).unwrap_or(J::Null));
            }
            close_notify.notify_waiters();
            Ok(true)
        })?,
    )?;
    Ok(handle)
}

const FORWARDED_UI_KEYS: &[&str] = &["select", "input", "confirm", "connected", "can_respond", "editor", "questions"];

fn current_session_id(host: &Host, fallback: &str) -> String {
    host.kernel.event_log().session_identity().0.filter(|s| !s.is_empty()).unwrap_or_else(|| fallback.to_owned())
}

/// Install RPC shell state mirroring and contract shell helpers once per `Host`.
pub(super) fn install_shell_mirror_on(rpc: &Rc<RpcRuntime>, session_id: &str, host: &Arc<Host>) -> mlua::Result<()> {
    if !host.shell.try_mark_mirror_installed() {
        return Ok(());
    }
    let lua = host.lua();
    let ui: Table = lua.globals().get::<Table>("mare")?.get("ui")?;
    let sid_owned = session_id.to_string();

    let wh = Arc::downgrade(host);
    ui.set(
        "print",
        lua.create_function(move |lua, v: Value| {
            // Validate & encode wire Node and emit projected `ui/print` without stderr fallback in `--rpc`.
            if let Some(host) = wh.upgrade() {
                let wire = encode_node_or_string_json(lua, v.clone())?;
                let text = mare_binding::ui::render_node_text(&host.ui, v, 80)?;
                host.spawn_ui_print_node(&text, wire);
            }
            Ok(())
        })?,
    )?;

    let wr = Rc::downgrade(rpc);
    let wh = Arc::downgrade(host);
    let n_sid = sid_owned.clone();
    ui.set(
        "notify",
        lua.create_function(move |_, (text, level): (String, Option<String>)| {
            if let (Some(rpc), Some(host)) = (wr.upgrade(), wh.upgrade()) {
                let sid = current_session_id(&host, &n_sid);
                rpc.broadcast_ui(
                    &host,
                    methods::UI_NOTIFY,
                    serde_json::to_value(proto::UiNotifyParams { session_id: Some(sid), text, level })
                        .unwrap_or(J::Null),
                );
            }
            Ok(())
        })?,
    )?;

    let wr = Rc::downgrade(rpc);
    let wh = Arc::downgrade(host);
    let se_sid = sid_owned.clone();
    ui.set(
        "set_editor",
        lua.create_function(move |_, (text, opts): (String, Option<Table>)| {
            if let (Some(rpc), Some(host)) = (wr.upgrade(), wh.upgrade()) {
                let sid = current_session_id(&host, &se_sid);
                let (keep_draft, prepend) = match &opts {
                    Some(o) => (
                        o.get::<Option<bool>>("keep_draft")?.unwrap_or(false),
                        o.get::<Option<bool>>("prepend")?.unwrap_or(false),
                    ),
                    None => (false, false),
                };
                let markers: Vec<String> = opts
                    .as_ref()
                    .and_then(|o| o.get::<Option<Table>>("image_markers").ok().flatten())
                    .map(|t| t.sequence_values::<String>().collect::<mlua::Result<_>>())
                    .transpose()?
                    .unwrap_or_default();
                super::ui_bridge::validate_editor_attachments(Some(&markers), None)
                    .map_err(|e| mlua::Error::runtime(e.message()))?;
                let (_changed, state) =
                    host.shell.apply_set_editor(Some(&sid), &text, keep_draft, prepend, markers, Vec::new());
                rpc.broadcast_ui(
                    &host,
                    methods::UI_SET_EDITOR,
                    serde_json::to_value(proto::UiSetEditorParams {
                        session_id: Some(sid.clone()),
                        text,
                        keep_draft,
                        prepend,
                    })
                    .unwrap_or(J::Null),
                );
                rpc.broadcast_editor_state(&sid, &host, &state);
            }
            Ok(())
        })?,
    )?;

    let wr = Rc::downgrade(rpc);
    let wh = Arc::downgrade(host);
    let eh_sid = sid_owned.clone();
    ui.set(
        "editor_hint",
        lua.create_function(move |_, text: String| {
            if let (Some(rpc), Some(host)) = (wr.upgrade(), wh.upgrade()) {
                let sid = current_session_id(&host, &eh_sid);
                host.shell.set_editor_hint(&text);
                rpc.broadcast_ui(
                    &host,
                    methods::UI_EDITOR_HINT,
                    serde_json::to_value(proto::UiEditorHintParams { session_id: Some(sid), text }).unwrap_or(J::Null),
                );
            }
            Ok(())
        })?,
    )?;

    let wr = Rc::downgrade(rpc);
    let wh = Arc::downgrade(host);
    let ec_sid = sid_owned.clone();
    ui.set(
        "editor_config",
        lua.create_function(move |_, t: Table| {
            if let (Some(rpc), Some(host)) = (wr.upgrade(), wh.upgrade()) {
                let sid = current_session_id(&host, &ec_sid);
                let params = proto::UiEditorConfigParams {
                    session_id: Some(sid),
                    prompt: t.get("prompt")?,
                    placeholder: t.get("placeholder")?,
                    busy_placeholder: t.get("busy_placeholder")?,
                    border: t.get("border")?,
                };
                host.shell.set_editor_config(&params);
                rpc.broadcast_ui(&host, methods::UI_EDITOR_CONFIG, serde_json::to_value(params).unwrap_or(J::Null));
            }
            Ok(())
        })?,
    )?;

    let wr = Rc::downgrade(rpc);
    let wh = Arc::downgrade(host);
    let wid_sid = sid_owned.clone();
    ui.set(
        "set_widget",
        lua.create_function(move |lua, (id, slot, v, order): (String, Option<String>, Value, Option<i64>)| {
            let slot = match slot.as_deref() {
                Some(name) => mare_shared::layout::validate_widget_slot(name).map_err(mlua::Error::RuntimeError)?,
                None => "above-editor",
            };
            if let (Some(rpc), Some(host)) = (wr.upgrade(), wh.upgrade()) {
                let sid = current_session_id(&host, &wid_sid);
                let node = encode_node_or_string_json(lua, v)?;
                let params = proto::UiWidgetParams {
                    session_id: Some(sid.clone()),
                    widget_id: id.clone(),
                    slot: slot.into(),
                    order: order.unwrap_or(0),
                    node,
                };
                host.shell.set_widget(&params);
                let payload = serde_json::to_value(&params).unwrap_or(J::Null);
                rpc.broadcast_component(&sid, &host, Some(methods::UI_WIDGET), &format!("widget:{id}"), payload);
            }
            Ok(())
        })?,
    )?;

    let wr = Rc::downgrade(rpc);
    let wh = Arc::downgrade(host);
    let st_sid = sid_owned.clone();
    ui.set(
        "set_status",
        lua.create_function(
            move |lua, (id, align, v, order, keep): (String, Option<String>, Value, Option<i64>, Option<bool>)| {
                let align = match align.as_deref() {
                    Some(name) => {
                        mare_shared::layout::validate_status_align(name).map_err(mlua::Error::RuntimeError)?
                    }
                    None => "left",
                };
                if let (Some(rpc), Some(host)) = (wr.upgrade(), wh.upgrade()) {
                    let sid = current_session_id(&host, &st_sid);
                    let node = encode_node_or_string_json(lua, v)?;
                    let params = proto::UiStatusParams {
                        session_id: Some(sid.clone()),
                        status_id: id,
                        align: align.into(),
                        order: order.unwrap_or(0),
                        keep: keep.unwrap_or(false),
                        node,
                    };
                    host.shell.set_status(&params);
                    let payload = serde_json::to_value(&params).unwrap_or(J::Null);
                    rpc.broadcast_component(&sid, &host, Some(methods::UI_STATUS), "status", payload);
                }
                Ok(())
            },
        )?,
    )?;

    let wr = Rc::downgrade(rpc);
    let wh = Arc::downgrade(host);
    let wk_sid = sid_owned.clone();
    ui.set(
        "working",
        lua.create_function(move |_, (label, detail, opts): (Option<String>, Option<String>, Option<Table>)| {
            let background = opts.and_then(|o| o.get::<Option<bool>>("background").ok().flatten()).unwrap_or(false);
            if let (Some(rpc), Some(host)) = (wr.upgrade(), wh.upgrade()) {
                let sid = current_session_id(&host, &wk_sid);
                let snap = host.shell.set_working(label, detail, background);
                let payload = snap
                    .map(|s| serde_json::to_value(s).unwrap_or(J::Null))
                    .unwrap_or_else(|| json!({ "label": J::Null, "detail": J::Null, "background": false }));
                rpc.broadcast_takeover(&sid, &host, "working", payload);
            }
            Ok(())
        })?,
    )?;

    let wr = Rc::downgrade(rpc);
    let wh = Arc::downgrade(host);
    let ov_sid = sid_owned.clone();
    ui.set(
        "overlay",
        lua.create_function(move |lua, (title, v, opts): (String, Value, Option<Table>)| {
            if let (Some(rpc), Some(host)) = (wr.upgrade(), wh.upgrade()) {
                let sid = current_session_id(&host, &ov_sid);
                let node = encode_node_or_string_json(lua, v)?;
                let open = node.is_some();
                let context = opts.and_then(|o| o.get::<Option<String>>("context").ok().flatten());
                let params = proto::UiOverlayParams { session_id: Some(sid.clone()), title, node, open, context };
                host.shell.set_overlay(&params);
                let payload = serde_json::to_value(&params).unwrap_or(J::Null);
                rpc.broadcast_component(&sid, &host, Some(methods::UI_OVERLAY), "overlay", payload);
            }
            Ok(())
        })?,
    )?;

    let wh = Arc::downgrade(host);
    ui.set(
        "flag",
        lua.create_function(move |_, (name, value): (String, bool)| {
            if let Some(host) = wh.upgrade() {
                host.shell.set_flag(&name, value);
            }
            Ok(())
        })?,
    )?;

    // Contract bridge helpers (`ui.frontend`) for `ui.update_editor`, `send_editor`, `set_external_editor`, `call_keymap`.
    let rpc_shell = lua.create_table()?;
    let wh = Arc::downgrade(host);
    rpc_shell.set(
        "editor_is_empty",
        lua.create_function(move |_, ()| Ok(wh.upgrade().is_none_or(|h| h.shell.editor_is_empty())))?,
    )?;
    let wh = Arc::downgrade(host);
    rpc_shell
        .set("is_working", lua.create_function(move |_, ()| Ok(wh.upgrade().is_some_and(|h| h.shell.is_working())))?)?;
    let wr = Rc::downgrade(rpc);
    let wh = Arc::downgrade(host);
    let ue_sid = sid_owned.clone();
    rpc_shell.set(
        "update_editor",
        lua.create_function(move |lua, (text, opts): (String, Value)| {
            let (Some(rpc), Some(host)) = (wr.upgrade(), wh.upgrade()) else {
                return Ok(Value::Nil);
            };
            let live_sid = current_session_id(&host, &ue_sid);
            let opts_json = mare_shared::lua::strict_lua_to_json(lua, opts)?;
            let res = super::ui_bridge::rpc_update_editor(&rpc, &host, &live_sid, text, opts_json);
            json_to_lua(lua, &res)
        })?,
    )?;
    let wr = Rc::downgrade(rpc);
    let wh = Arc::downgrade(host);
    let se_sid = sid_owned.clone();
    rpc_shell.set(
        "send_editor",
        lua.create_async_function(
            move |lua, (text, mode, atts, allow_empty): (Option<String>, String, Value, Option<bool>)| {
                let wr = wr.clone();
                let wh = wh.clone();
                let se_sid = se_sid.clone();
                async move {
                    let (Some(rpc), Some(host)) = (wr.upgrade(), wh.upgrade()) else {
                        return Ok(Value::Nil);
                    };
                    let live_sid = current_session_id(&host, &se_sid);
                    let raw_atts = if matches!(atts, Value::Nil) {
                        None
                    } else {
                        Some(mare_shared::lua::strict_lua_to_json(&lua, atts)?)
                    };
                    let res = super::ui_bridge::rpc_send_editor(
                        &rpc,
                        &host,
                        &live_sid,
                        text,
                        mode,
                        raw_atts,
                        allow_empty.unwrap_or(false),
                    )
                    .await;
                    json_to_lua(&lua, &res)
                }
            },
        )?,
    )?;
    let wr = Rc::downgrade(rpc);
    let wh = Arc::downgrade(host);
    let ee_sid = sid_owned;
    rpc_shell.set(
        "set_external_editor",
        lua.create_function(move |lua, (action, text): (String, Option<String>)| {
            let (Some(rpc), Some(host)) = (wr.upgrade(), wh.upgrade()) else {
                return Ok(Value::Nil);
            };
            let live_sid = current_session_id(&host, &ee_sid);
            let res = super::ui_bridge::rpc_set_external_editor(&rpc, &host, &live_sid, &action, text);
            json_to_lua(lua, &res)
        })?,
    )?;
    let wr = Rc::downgrade(rpc);
    let wh = Arc::downgrade(host);
    rpc_shell.set(
        "get_select_preview",
        lua.create_async_function(
            move |lua, (caller_id, select_id, index, tab): (Option<u64>, String, usize, usize)| {
                let wr = wr.clone();
                let wh = wh.clone();
                async move {
                    let (Some(rpc), Some(host)) = (wr.upgrade(), wh.upgrade()) else {
                        return Ok(Value::Nil);
                    };
                    let Some(cb) = caller_id
                        .and_then(|cid| rpc.conn_id_for_ctx(&host, cid))
                        .and_then(|conn_id| host.answerer.get_active_select(conn_id, &select_id))
                    else {
                        return Ok(Value::Nil);
                    };
                    let v: Value = cb.call_async((index + 1, tab + 1)).await?;
                    let preview = mare_binding::ui_wire::encode_node_or_string_json(&lua, v)?;
                    let out = lua.create_table()?;
                    out.set("found", true)?;
                    if let Some(p) = preview {
                        out.set("preview", json_to_lua(&lua, &p)?)?;
                    }
                    Ok(Value::Table(out))
                }
            },
        )?,
    )?;
    ui.set("frontend", rpc_shell)?;

    Ok(())
}

/// Forward interactive `mare.ui.*` responder callbacks to `conn` while connected.
pub(super) fn forward_ui_on(conn: &Rc<Conn>, session_id: &str, host: &Arc<Host>, _ctx_id: u64) -> mlua::Result<()> {
    install_shell_mirror_on(&conn.rpc, session_id, host)?;
    if host.answerer.has_saved_ui(conn.id) {
        return Ok(());
    }
    let lua = host.lua();
    let ui: Table = lua.globals().get::<Table>("mare")?.get("ui")?;
    let mut saved = Vec::with_capacity(FORWARDED_UI_KEYS.len());
    for &k in FORWARDED_UI_KEYS {
        saved.push((k, ui.get::<Value>(k)?));
    }
    host.answerer.set_saved_ui(conn.id, ui.clone(), saved);

    let w = Rc::downgrade(conn);
    let wh = Arc::downgrade(host);
    ui.set(
        "connected",
        lua.create_function(move |_, ()| {
            Ok(matches!((w.upgrade(), wh.upgrade()), (Some(c), Some(h)) if !ui_unavailable(&c, &h)))
        })?,
    )?;

    let w = Rc::downgrade(conn);
    let wh = Arc::downgrade(host);
    ui.set(
        "can_respond",
        lua.create_function(move |_, ()| {
            Ok(matches!((w.upgrade(), wh.upgrade()), (Some(c), Some(h)) if !ui_unavailable(&c, &h)))
        })?,
    )?;

    let session_id = session_id.to_owned();
    let w = Rc::downgrade(conn);
    let wh = Arc::downgrade(host);
    let sid_cap = session_id.clone();
    ui.set(
        "select",
        lua.create_async_function(move |lua, t: Table| {
            let w = w.clone();
            let wh = wh.clone();
            let sid_cap = sid_cap.clone();
            async move {
                let (Some(conn), Some(host)) = (w.upgrade(), wh.upgrade()) else {
                    return Ok((None, Some(Value::String(lua.create_string("unavailable")?)), None, None, None));
                };
                if ui_unavailable(&conn, &host) {
                    return Ok((None, Some(Value::String(lua.create_string("unavailable")?)), None, None, None));
                }
                let items = parse_select_items(&lua, t.get::<Option<Table>>("items")?)?;
                let tabs = parse_select_tabs(&lua, t.get::<Option<Table>>("tabs")?)?;
                let body = match t.get::<Value>("body")? {
                    Value::String(b) => b.to_str()?.lines().map(|l| J::String(l.to_string())).collect(),
                    Value::Table(lines) => {
                        let mut out = vec![];
                        for lv in lines.sequence_values::<Value>() {
                            if let Some(w) = encode_node_or_string_json(&lua, lv?)? {
                                out.push(w);
                            }
                        }
                        out
                    }
                    _ => vec![],
                };
                let title_right = encode_node_or_string_json(&lua, t.get::<Value>("title_right")?)?;
                let mut keys = vec![];
                if let Some(ks) = t.get::<Option<Table>>("keys")? {
                    for kv in ks.sequence_values::<Value>() {
                        match kv? {
                            Value::String(s) => {
                                keys.push(proto::UiSelectKey { key: s.to_str()?.to_string(), label: None })
                            }
                            Value::Table(kt) => keys.push(proto::UiSelectKey {
                                key: kt.get::<Option<String>>("key")?.unwrap_or_default(),
                                label: kt.get("label")?,
                            }),
                            _ => {}
                        }
                    }
                }
                let on_highlight = t.get::<Option<Function>>("on_highlight")?;
                let on_open = t.get::<Option<Function>>("on_open")?;
                let select_id = if on_highlight.is_some() || on_open.is_some() {
                    let n = conn.next_id.get();
                    conn.next_id.set(n + 1);
                    Some(format!("sel-{n}"))
                } else {
                    None
                };
                let open = Rc::new(Cell::new(true));
                if let (Some(sid), Some(cb)) = (select_id.as_ref(), on_highlight.as_ref()) {
                    host.answerer.insert_active_select(conn.id, sid.clone(), cb.clone());
                }
                let _guard = SelectGuard { conn: &conn, host: &host, select_id: select_id.clone(), open: open.clone() };

                let live_sid = current_session_id(&host, &sid_cap);
                let params = proto::UiSelectParams {
                    session_id: Some(live_sid.clone()),
                    select_id: select_id.clone(),
                    title: t.get::<Option<String>>("title")?.unwrap_or_default(),
                    title_right,
                    body,
                    items,
                    selected: t.get::<Option<usize>>("selected")?.unwrap_or(1).saturating_sub(1),
                    multi: t.get::<Option<bool>>("multi")?.unwrap_or(false),
                    accept_label: t.get("accept_label")?,
                    allow_search: Some(t.get::<Option<bool>>("allow_search")?.unwrap_or(true)),
                    filter: t.get("filter")?,
                    tabs,
                    tab: t.get::<Option<usize>>("tab")?.unwrap_or(1).saturating_sub(1),
                    kind: t.get("kind")?,
                    keys,
                    vim: t.get("vim")?,
                    has_highlight: on_highlight.as_ref().map(|_| true),
                };
                let cancel = parse_cancel(&t)?;
                if cancel.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
                    return Ok((None, Some(Value::String(lua.create_string("cancelled")?)), None, None, None));
                }
                let payload = serde_json::to_value(params).unwrap_or(J::Null);
                let r = if let (Some(f), Some(sid)) = (on_open, select_id) {
                    if ui_unavailable(&conn, &host) {
                        Err("unavailable")
                    } else {
                        let cap_rev = conn.cap_revision.get();
                        let close_notify = Rc::new(tokio::sync::Notify::new());
                        let handle = make_select_handle(&lua, &conn, &host, sid, open.clone(), close_notify.clone())?;
                        let call_fut = conn.call_with_timeout_for_session(methods::UI_SELECT, &live_sid, payload, None);
                        tokio::pin!(call_fut);
                        let open_fut = f.call_async::<()>(handle);
                        let res = tokio::select! {
                            biased;
                            _ = cancel_or_pending(cancel.as_ref()) => Err(()),
                            _ = close_notify.notified() => Err(()),
                            res = &mut call_fut => Ok(res),
                            open_res = open_fut => {
                                open_res?;
                                if !open.get() {
                                    Err(())
                                } else {
                                    tokio::select! {
                                        _ = cancel_or_pending(cancel.as_ref()) => Err(()),
                                        _ = close_notify.notified() => Err(()),
                                        res = call_fut => Ok(res),
                                    }
                                }
                            }
                        };
                        if conn.cap_revision.get() != cap_rev || ui_unavailable(&conn, &host) {
                            Err("unavailable")
                        } else {
                            map_call_result(res)
                        }
                    }
                } else {
                    call_ui(Some(&conn), Some(&host), &live_sid, methods::UI_SELECT, payload, cancel.as_ref()).await
                };
                let v = match r {
                    Ok(v) => v,
                    Err(reason) => {
                        return Ok((None, Some(Value::String(lua.create_string(reason)?)), None, None, None));
                    }
                };
                let r: proto::UiSelectResult = serde_json::from_value(v).unwrap_or_default();
                let multi = t.get::<Option<bool>>("multi")?.unwrap_or(false);
                if (multi && r.selected_indices.is_none()) || (!multi && r.index.is_none() && r.key.is_none()) {
                    let reason = dismiss_reason(r.reason.as_deref());
                    return Ok((None, Some(Value::String(lua.create_string(reason)?)), None, None, None));
                }
                let selected = if multi {
                    let indices =
                        lua.create_sequence_from(r.selected_indices.unwrap_or_default().into_iter().map(|i| i + 1))?;
                    indices.set_metatable(Some(lua.array_metatable()))?;
                    Some(Value::Table(indices))
                } else {
                    r.index.map(|i| Value::Integer((i + 1) as i64))
                };
                Ok((selected, r.tab.map(|t| Value::Integer((t + 1) as i64)), r.key, r.filter, r.item_id))
            }
        })?,
    )?;

    let w = Rc::downgrade(conn);
    let wh = Arc::downgrade(host);
    let sid_cap = session_id.clone();
    ui.set(
        "input",
        lua.create_async_function(move |_, t: Table| {
            let w = w.clone();
            let wh = wh.clone();
            let sid_cap = sid_cap.clone();
            async move {
                let cancel = parse_cancel(&t)?;
                let live_sid =
                    wh.upgrade().map(|h| current_session_id(&h, &sid_cap)).unwrap_or_else(|| sid_cap.clone());
                let params = proto::UiInputParams {
                    session_id: Some(live_sid.clone()),
                    title: t.get::<Option<String>>("title")?.unwrap_or_default(),
                    placeholder: t.get::<Option<String>>("placeholder")?.unwrap_or_default(),
                    default: t.get("default")?,
                    secret: t.get::<Option<bool>>("secret")?.unwrap_or(false),
                };
                Ok(rpc_text_prompt(
                    w.upgrade(),
                    wh.upgrade(),
                    &live_sid,
                    methods::UI_INPUT,
                    serde_json::to_value(params).unwrap_or(J::Null),
                    cancel,
                )
                .await)
            }
        })?,
    )?;

    let w = Rc::downgrade(conn);
    let wh = Arc::downgrade(host);
    let sid_cap = session_id.clone();
    ui.set(
        "editor",
        lua.create_async_function(move |_, t: Table| {
            let w = w.clone();
            let wh = wh.clone();
            let sid_cap = sid_cap.clone();
            async move {
                let cancel = parse_cancel(&t)?;
                let live_sid =
                    wh.upgrade().map(|h| current_session_id(&h, &sid_cap)).unwrap_or_else(|| sid_cap.clone());
                let params = proto::UiEditorParams {
                    session_id: Some(live_sid.clone()),
                    title: t.get::<Option<String>>("title")?.unwrap_or_default(),
                    prefill: t.get::<Option<String>>("prefill")?.unwrap_or_default(),
                };
                struct CallbackEditorGuard {
                    wh: std::sync::Weak<Host>,
                    rpc: std::rc::Weak<RpcRuntime>,
                    sid_cap: String,
                    active: bool,
                }
                impl Drop for CallbackEditorGuard {
                    fn drop(&mut self) {
                        if !self.active {
                            return;
                        }
                        if let Some(h) = self.wh.upgrade() {
                            let cur_sid = current_session_id(&h, &self.sid_cap);
                            if let Ok(st) = h.shell.set_callback_editor_active(Some(&cur_sid), false)
                                && let Some(rpc) = self.rpc.upgrade()
                            {
                                rpc.broadcast_editor_state(&cur_sid, &h, &st);
                            }
                        }
                    }
                }
                let mut guard = CallbackEditorGuard {
                    wh: wh.clone(),
                    rpc: w.upgrade().map(|c| Rc::downgrade(&c.rpc)).unwrap_or_default(),
                    sid_cap: sid_cap.clone(),
                    active: false,
                };
                if let (Some(c), Some(h)) = (w.upgrade(), wh.upgrade()) {
                    match h.shell.set_callback_editor_active(Some(&live_sid), true) {
                        Ok(st) => {
                            guard.active = true;
                            c.rpc.broadcast_editor_state(&live_sid, &h, &st);
                        }
                        Err(msg) => return Ok((None, Some(msg.to_string()))),
                    }
                }
                let res = rpc_text_prompt(
                    w.upgrade(),
                    wh.upgrade(),
                    &live_sid,
                    methods::UI_EDITOR,
                    serde_json::to_value(params).unwrap_or(J::Null),
                    cancel,
                )
                .await;
                drop(guard);
                Ok(res)
            }
        })?,
    )?;

    let w = Rc::downgrade(conn);
    let wh = Arc::downgrade(host);
    let sid_cap = session_id.clone();
    ui.set(
        "questions",
        lua.create_async_function(move |lua, t: Table| {
            let w = w.clone();
            let wh = wh.clone();
            let sid_cap = sid_cap.clone();
            async move {
                let (Some(conn), Some(host)) = (w.upgrade(), wh.upgrade()) else {
                    return Ok((Value::Nil, Some("unavailable".to_string())));
                };
                if ui_unavailable(&conn, &host) {
                    return Ok((Value::Nil, Some("unavailable".to_string())));
                }
                let mut questions = vec![];
                if let Some(list) = t.get::<Option<Table>>("questions")? {
                    for qv in list.sequence_values::<Value>() {
                        let Value::Table(qt) = qv? else { continue };
                        let mut options = vec![];
                        if let Some(opts) = qt.get::<Option<Table>>("options")? {
                            for ov in opts.sequence_values::<Value>() {
                                let Value::Table(ot) = ov? else { continue };
                                options.push(proto::UiQuestionOption {
                                    label: ot.get::<Option<String>>("label")?.unwrap_or_default(),
                                    description: ot.get("description")?,
                                    recommended: ot.get("recommended")?,
                                });
                            }
                        }
                        let preview = encode_node_or_string_json(&lua, qt.get::<Value>("preview")?)?;
                        let question_id = qt.get::<Option<String>>("question_id")?.unwrap_or_default();
                        questions.push(proto::UiQuestionItem {
                            question_id,
                            header: qt.get("header")?,
                            question: qt.get::<Option<String>>("question")?.unwrap_or_default(),
                            options,
                            multi_select: qt.get("multi_select")?,
                            allow_custom: qt.get("allow_custom")?,
                            allow_skip: qt.get("allow_skip")?,
                            preview,
                        });
                    }
                }
                let cancel = parse_cancel(&t)?;
                let live_sid = current_session_id(&host, &sid_cap);
                let params =
                    proto::UiQuestionsParams { session_id: Some(live_sid.clone()), title: t.get("title")?, questions };
                let r = match call_ui(
                    Some(&conn),
                    Some(&host),
                    &live_sid,
                    methods::UI_QUESTIONS,
                    serde_json::to_value(params).unwrap_or(J::Null),
                    cancel.as_ref(),
                )
                .await
                {
                    Ok(v) => v,
                    Err(reason) => return Ok((Value::Nil, Some(reason.to_string()))),
                };
                let Ok(mut res) = serde_json::from_value::<proto::UiQuestionsResult>(r) else {
                    return Ok((Value::Nil, Some("ui.questions: invalid RPC response".to_string())));
                };
                if res.dismissed == Some(true) {
                    res.reason = Some(dismiss_reason(res.reason.as_deref()).to_string());
                }
                let v = json_to_lua(&lua, &serde_json::to_value(res).unwrap_or(J::Null))?;
                Ok((v, None))
            }
        })?,
    )?;

    let w = Rc::downgrade(conn);
    let wh = Arc::downgrade(host);
    let sid_cap = session_id;
    ui.set(
        "confirm",
        lua.create_async_function(move |_, (prompt_or_tbl, default, opts): (Value, Option<bool>, Option<Table>)| {
            let w = w.clone();
            let wh = wh.clone();
            let sid_cap = sid_cap.clone();
            async move {
                let (Some(conn), Some(host)) = (w.upgrade(), wh.upgrade()) else {
                    return Ok((Value::Nil, Some("unavailable".to_string())));
                };
                if ui_unavailable(&conn, &host) {
                    return Ok((Value::Nil, Some("unavailable".to_string())));
                }
                let (prompt, default, cancel) = match prompt_or_tbl {
                    Value::Table(t) => {
                        let p = t
                            .get::<Option<String>>("prompt")?
                            .or(t.get::<Option<String>>("title")?)
                            .unwrap_or_default();
                        let d = t.get::<Option<bool>>("default")?;
                        let c = parse_cancel(&t)?;
                        (p, d, c)
                    }
                    Value::String(s) => {
                        let c = opts.as_ref().map(parse_cancel).transpose()?.flatten();
                        (s.to_str()?.to_string(), default, c)
                    }
                    _ => return Ok((Value::Boolean(false), None)),
                };
                let live_sid = current_session_id(&host, &sid_cap);
                let params = proto::UiConfirmParams { session_id: Some(live_sid.clone()), prompt, default };
                let v = match call_ui(
                    Some(&conn),
                    Some(&host),
                    &live_sid,
                    methods::UI_CONFIRM,
                    serde_json::to_value(params).unwrap_or(J::Null),
                    cancel.as_ref(),
                )
                .await
                {
                    Ok(v) => v,
                    Err(reason) => return Ok((Value::Nil, Some(reason.to_string()))),
                };
                let r: proto::UiConfirmResult = serde_json::from_value(v).unwrap_or_default();
                match r.confirmed {
                    Some(b) if r.reason.as_deref() != Some("interrupted") => Ok((Value::Boolean(b), None)),
                    _ => Ok((Value::Nil, Some(dismiss_reason(r.reason.as_deref()).to_string()))),
                }
            }
        })?,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mare_shared::ui::UiCmd;

    /// Classification of every core `UiCmd` for RPC / external frontend boundaries.
    /// Exhaustive match intentionally has no wildcard so adding a `UiCmd` forces an explicit decision.
    #[derive(Debug, PartialEq, Eq)]
    enum RpcUiBoundary {
        SessionProjection(&'static str),
        RpcRequest(&'static str),
        RpcNotification(&'static str),
        LocalTuiOnly,
    }

    fn classify_ui_cmd(cmd: &UiCmd) -> RpcUiBoundary {
        match cmd {
            UiCmd::BlockPush { .. }
            | UiCmd::BlockUpdate { .. }
            | UiCmd::BlockFinish { .. }
            | UiCmd::BlockRemove { .. } => RpcUiBoundary::SessionProjection("item/*"),
            UiCmd::Widget { .. } => RpcUiBoundary::RpcNotification(methods::UI_WIDGET),
            UiCmd::Status { .. } => RpcUiBoundary::RpcNotification(methods::UI_STATUS),
            UiCmd::Working { .. } => RpcUiBoundary::SessionProjection("status/changed"),
            UiCmd::Notify { .. } => RpcUiBoundary::RpcNotification(methods::UI_NOTIFY),
            UiCmd::Select { .. } => RpcUiBoundary::RpcRequest(methods::UI_SELECT),
            UiCmd::SelectUpdate { .. } => RpcUiBoundary::RpcNotification(methods::UI_SELECT_UPDATE),
            UiCmd::SelectPreview { .. } => RpcUiBoundary::RpcRequest("ui.get_select_preview"),
            UiCmd::Input { .. } => RpcUiBoundary::RpcRequest(methods::UI_INPUT),
            UiCmd::CancelModal { .. } => RpcUiBoundary::RpcNotification(methods::UI_SELECT_CLOSE),
            UiCmd::SetEditor { .. } => RpcUiBoundary::RpcNotification(methods::UI_SET_EDITOR),
            UiCmd::EditorConfig { .. } => RpcUiBoundary::RpcNotification(methods::UI_EDITOR_CONFIG),
            UiCmd::EditorHint { .. } => RpcUiBoundary::RpcNotification(methods::UI_EDITOR_HINT),
            UiCmd::Print { .. } => RpcUiBoundary::SessionProjection("ui/print"),
            UiCmd::Overlay { .. } => RpcUiBoundary::RpcNotification(methods::UI_OVERLAY),
            UiCmd::TranscriptView { .. }
            | UiCmd::TranscriptClose { .. }
            | UiCmd::Flag { .. }
            | UiCmd::SetBindings { .. }
            | UiCmd::SetBindingsBatch { .. }
            | UiCmd::SetContext { .. }
            | UiCmd::SetContextActive { .. }
            | UiCmd::EditorOp(_)
            | UiCmd::SyncEditorChanged
            | UiCmd::CompletionSourceRemoved { .. }
            | UiCmd::ThemeChanged
            | UiCmd::Redraw
            | UiCmd::ColorMode(_)
            | UiCmd::NotifyOs { .. }
            | UiCmd::Title { .. }
            | UiCmd::Clear
            | UiCmd::Exit
            | UiCmd::ExitForce
            | UiCmd::ExitGuard { .. }
            | UiCmd::Layout(_) => RpcUiBoundary::LocalTuiOnly,
        }
    }

    #[test]
    fn rpc_ui_cmd_exhaustive_guard_and_forwarded_keys() {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        assert_eq!(
            classify_ui_cmd(&UiCmd::Input {
                title: "t".into(),
                placeholder: String::new(),
                default: String::new(),
                secret: false,
                reply: tx,
                modal_id: 1,
            }),
            RpcUiBoundary::RpcRequest(methods::UI_INPUT)
        );
        assert_eq!(
            classify_ui_cmd(&UiCmd::Print { node: mare_shared::ui::Node::Empty }),
            RpcUiBoundary::SessionProjection("ui/print")
        );
        assert_eq!(classify_ui_cmd(&UiCmd::ThemeChanged), RpcUiBoundary::LocalTuiOnly);
        assert!(FORWARDED_UI_KEYS.contains(&"confirm"));
        assert!(FORWARDED_UI_KEYS.contains(&"questions"));
        assert!(FORWARDED_UI_KEYS.contains(&"select"));
        assert!(FORWARDED_UI_KEYS.contains(&"input"));
        assert!(FORWARDED_UI_KEYS.contains(&"editor"));
        assert_eq!(dismiss_reason(Some("interrupted")), "interrupted");
        assert_eq!(dismiss_reason(Some("cancelled")), "cancelled");
        assert_eq!(dismiss_reason(None), "cancelled");
    }
}
