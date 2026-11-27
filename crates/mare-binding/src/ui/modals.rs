//! Modal prompts (`select`, `approve`, `input`, `editor`, `questions`) and cancellation helpers.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use mlua::{Lua, LuaSerdeExt, Table, Value};
use tokio::sync::oneshot;

use mare_shared::style::Span;
use mare_shared::ui::Dismissed;

use super::{SelectItem, UiCmd, UiShared, node_from, spans_from};

pub(super) fn install(lua: &Lua, ui: &Table, shared: &Arc<UiShared>) -> mlua::Result<()> {
    macro_rules! sh {
        () => {
            shared.clone()
        };
    }

    // select{...} -> index or checked-index array, plus tab/key/filter; nil on dismissal.
    let s = sh!();
    ui.set(
        "select",
        lua.create_async_function(move |lua, t: Table| {
            let s = s.clone();
            async move {
                let _ = &lua;
                let title: String = t.get::<Option<String>>("title")?.unwrap_or_default();
                let theme = s.theme.read().unwrap().clone();
                // title_right = "Sort: Updated" (muted) | a ui.Line the plugin styled itself
                let title_right = match t.get::<Value>("title_right")? {
                    Value::Nil => vec![],
                    Value::String(r) => vec![Span::styled(r.to_str()?.to_string(), theme.style("MareMuted"))],
                    v => spans_from(&theme, v)?,
                };
                // split = { ratio = 0.5, min = 76 }: the list's share of a wide popup
                let mut split = mare_shared::ui::SelectSplit::default();
                if let Some(sp) = t.get::<Option<Table>>("split")? {
                    if let Some(r) = sp.get::<Option<f32>>("ratio")? {
                        split.ratio = r.clamp(0.1, 0.9);
                    }
                    if let Some(m) = sp.get::<Option<usize>>("min")? {
                        split.min = m;
                    }
                }
                let parse_items = |list: Option<Table>| crate::ui_select::parse_items(&theme, list);
                let selected = t.get::<Option<usize>>("selected")?.unwrap_or(1).saturating_sub(1);
                let multi = t.get::<Option<bool>>("multi")?.unwrap_or(false);
                let accept_label = t.get::<Option<String>>("accept_label")?;
                let allow_search = t.get::<Option<bool>>("allow_search")?.unwrap_or(true);
                let items = parse_items(t.get("items")?)?;
                // tabs = { {name=, items=, selected=}, ... }, tab = 1-based initial tab
                let tabs = crate::ui_select::parse_tabs(&theme, t.get("tabs")?)?;
                let tab = t.get::<Option<usize>>("tab")?.unwrap_or(1).saturating_sub(1);
                let kind = t.get::<Option<String>>("kind")?.unwrap_or_else(|| "list".into());
                let filter: String = t.get::<Option<String>>("filter")?.unwrap_or_default();
                // keys = { {key="w", label="write to file"}, ... } (or plain "w")
                let mut keys = vec![];
                if let Some(list) = t.get::<Option<Table>>("keys")? {
                    for v in list.sequence_values::<Value>() {
                        let (k, label) = match v? {
                            Value::String(s) => (s.to_str()?.to_string(), String::new()),
                            Value::Table(it) => (
                                it.get::<Option<String>>("key")?.unwrap_or_default(),
                                it.get::<Option<String>>("label")?.unwrap_or_default(),
                            ),
                            _ => continue,
                        };
                        let normalized = s.normalize_key(&k);
                        if mare_shared::bindings::is_reserved(&normalized) {
                            return Err(mlua::Error::runtime(format!(
                                "mare.ui.select: {normalized} is reserved by the core"
                            )));
                        }
                        // "tab": Tab (works while searching too); "ctrl+d": kept as the control
                        // character (works while searching)
                        if k.eq_ignore_ascii_case("tab") {
                            keys.push(('\t', label));
                        } else if let Some(c) = k.strip_prefix("ctrl+").and_then(|r| {
                            let mut it = r.chars();
                            it.next().filter(|c| c.is_ascii_alphabetic() && it.next().is_none())
                        }) {
                            keys.push((((c.to_ascii_lowercase() as u8) & 0x1f) as char, label));
                        } else if let Some(c) = k.chars().next().filter(|_| k.chars().count() == 1) {
                            keys.push((c, label));
                        }
                    }
                }
                // body = "text" | { "line", ... }: explanatory lines above the options
                let body: Vec<String> = match t.get::<Value>("body")? {
                    Value::String(b) => b.to_str()?.lines().map(str::to_string).collect(),
                    Value::Table(l) => l.sequence_values::<String>().collect::<mlua::Result<_>>()?,
                    _ => vec![],
                };
                let vim = t.get::<Option<bool>>("vim")?.unwrap_or(false);
                // on_highlight(index, tab) -> node | nil: a preview pane computed only for the
                // item the user is on (1-based, like the reply)
                let on_highlight = t.get::<Option<mlua::Function>>("on_highlight")?;
                let (htx, hrx) = tokio::sync::mpsc::unbounded_channel::<(usize, usize, u64)>();
                let highlight = on_highlight.is_some().then_some(htx);
                // on_open(sel): the handle whose :update(items) replaces the items in place
                let on_open = t.get::<Option<mlua::Function>>("on_open")?;
                let cancel = parse_cancel(&t)?;
                if !s.can_respond() {
                    return Ok((None, Some(Value::String(lua.create_string("unavailable")?)), None, None));
                }
                if cancel.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
                    return Ok((None, Some(Value::String(lua.create_string("cancelled")?)), None, None));
                }
                let token = if on_open.is_some() { s.next_id.fetch_add(1, Ordering::Relaxed) } else { 0 };
                let modal_id = s.next_id.fetch_add(1, Ordering::Relaxed);
                let open = Arc::new(std::sync::atomic::AtomicBool::new(true));
                let (tx, mut rx) = oneshot::channel();
                let cmd = UiCmd::Select {
                    title,
                    title_right,
                    split,
                    items,
                    selected,
                    multi,
                    accept_label,
                    allow_search,
                    filter,
                    tabs,
                    tab,
                    kind,
                    keys,
                    body,
                    vim,
                    highlight,
                    reply: tx,
                    token,
                    modal_id,
                    approval: false,
                };
                if !s.send(cmd) {
                    open.store(false, Ordering::SeqCst);
                    return Ok((None, Some(Value::String(lua.create_string("unavailable")?)), None, None));
                }
                let mut guard = ModalCancelGuard { ui: s.clone(), modal_id, armed: true, open: Some(open) };
                let mut early_reply: Option<Result<Result<mare_shared::ui::SelectReply, Dismissed>, ()>> = None;
                if let Some(f) = &on_open {
                    let handle = crate::ui_select::SelectHandle {
                        token,
                        ui: s.clone(),
                        open: guard.open.as_ref().expect("set").clone(),
                    };
                    let open_fut = f.call_async::<()>(handle);
                    tokio::select! {
                        biased;
                        _ = cancel_or_pending(cancel.as_ref()) => {
                            return Ok((None, Some(Value::String(lua.create_string("cancelled")?)), None, None));
                        }
                        r = &mut rx => {
                            guard.armed = false;
                            early_reply = Some(Ok(r.unwrap_or(Err(Dismissed::Cancelled))));
                        }
                        res = open_fut => {
                            res?;
                        }
                    }
                }
                let reply_res = match early_reply {
                    Some(r) => r,
                    None => {
                        wait_select_with_guard(&s, &mut guard, rx, on_highlight.as_ref(), Some(hrx), cancel.as_ref())
                            .await
                    }
                };
                drop(guard);
                // -> index (1-based), tab (1-based), key (when an extra key chose the item), and
                // the search text then (re-open a picker after an action without losing it)
                Ok(match reply_res {
                    Err(()) => (None, Some(Value::String(lua.create_string("cancelled")?)), None, None),
                    Ok(Ok(r)) => {
                        // a Ctrl key comes back as "ctrl+d", the way it was asked for
                        let key = r.key.map(|c| {
                            if c == '\t' {
                                "tab".to_string()
                            } else if (c as u32) < 0x20 {
                                format!("ctrl+{}", ((c as u8) | 0x60) as char)
                            } else {
                                c.to_string()
                            }
                        });
                        let selected = if multi {
                            let indices = lua.create_sequence_from(r.selected_indices.into_iter().map(|i| i + 1))?;
                            indices.set_metatable(Some(lua.array_metatable()))?;
                            Value::Table(indices)
                        } else {
                            Value::Integer((r.index + 1) as i64)
                        };
                        (Some(selected), Some(Value::Integer((r.tab + 1) as i64)), key, Some(r.filter))
                    }
                    Ok(Err(reason)) => (None, Some(Value::String(lua.create_string(reason.as_str())?)), None, None),
                })
            }
        })?,
    )?;
    // Dedicated human-approval prompt: native code owns the
    // title, action summary, and choice/key mapping so no caller can disguise "Allow" as "Deny".
    let s = sh!();
    ui.set(
        "approve",
        lua.create_async_function(move |lua, t: Table| {
            let s = s.clone();
            async move {
                let binding_tbl = t
                    .get::<Option<Table>>("approval_binding")?
                    .ok_or_else(|| mlua::Error::runtime("approval_binding is required"))?;
                let binding = crate::approval::Binding::from_lua(&lua, binding_tbl)?;
                let snapshot = crate::approval::get_snapshot(&lua, &binding)
                    .ok_or_else(|| mlua::Error::runtime("unknown or expired approval binding"))?;
                if let Some(caller_args) = match t.get::<Value>("approval_arguments")? {
                    Value::Nil => None,
                    value => Some(mare_shared::lua::strict_lua_to_json(&lua, value)?),
                } && caller_args != snapshot.arguments
                {
                    return Err(mlua::Error::runtime("approval_arguments do not match immutable approval snapshot"));
                }
                if let Some(caller_tool) = t.get::<Option<String>>("tool")?
                    && caller_tool != snapshot.tool
                {
                    return Err(mlua::Error::runtime("approval tool does not match immutable approval snapshot"));
                }
                if let Some(caller_unconfined) = t.get::<Option<bool>>("unconfined")?
                    && caller_unconfined != snapshot.unconfined
                {
                    return Err(mlua::Error::runtime(
                        "approval unconfined flag does not match immutable approval snapshot",
                    ));
                }
                if let Some(caller_protected) = t.get::<Option<bool>>("protected_arguments")?
                    && caller_protected != snapshot.protected_arguments
                {
                    return Err(mlua::Error::runtime(
                        "approval protected_arguments flag does not match immutable approval snapshot",
                    ));
                }
                let caller_origin = match t.get::<Value>("origin")? {
                    Value::Nil => None,
                    value => Some(mare_shared::lua::strict_lua_to_json(&lua, value)?),
                };
                if let Some(caller_origin) = caller_origin
                    && snapshot.origin.as_ref() != Some(&caller_origin)
                {
                    return Err(mlua::Error::runtime("approval origin does not match immutable approval snapshot"));
                }
                let arguments = snapshot.arguments;
                let tool = snapshot.tool;
                let raw_choices =
                    t.get::<Table>("choices")?.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>()?;
                if raw_choices.is_empty() {
                    return Err(mlua::Error::runtime("approval choices must not be empty"));
                }
                if snapshot.protected_arguments
                    && raw_choices.iter().any(|c| matches!(c.as_str(), "allow_session" | "allow_amend"))
                {
                    return Err(mlua::Error::runtime(
                        "protected_arguments cannot be approved for session or rule amendment",
                    ));
                }
                let scoped = snapshot.scoped;
                let unconfined = snapshot.unconfined;
                let protected_arguments = snapshot.protected_arguments;
                let amendment_desc = snapshot.amendment_description;
                let reason = snapshot.reason;
                let who = if let Some(origin) = snapshot.origin {
                    let mut parts = vec![
                        origin
                            .get("label")
                            .and_then(|v| v.as_str())
                            .or_else(|| origin.get("agent_id").and_then(|v| v.as_str()))
                            .unwrap_or("sub-agent")
                            .to_string(),
                    ];
                    if let Some(run_id) = origin.get("run_id").and_then(|v| v.as_str()) {
                        parts.push(format!("run {run_id}"));
                    }
                    if let Some(phase) = origin.get("phase").and_then(|v| v.as_str()) {
                        parts.push(phase.to_string());
                    }
                    if let Some(task) = origin.get("task").and_then(|v| v.as_str()) {
                        parts.push(task.to_string());
                    }
                    format!("[{}] ", parts.join(" · "))
                } else {
                    String::new()
                };
                let arg_preview = arguments
                    .get("command")
                    .or_else(|| arguments.get("path"))
                    .and_then(|v| v.as_str())
                    .map(|raw| {
                        let mut s = format!(": {raw}");
                        if s.chars().count() > 120 {
                            s = format!("{}…", s.chars().take(119).collect::<String>());
                        }
                        s
                    })
                    .unwrap_or_default();
                let mut what = arg_preview;
                if unconfined {
                    what.push_str(" (runs outside the local sandbox)");
                }
                if protected_arguments {
                    what.push_str(" (hidden registered values restored at execution; approve once)");
                }
                let reason_suffix = reason.as_deref().map(|r| format!("  {r}")).unwrap_or_default();
                let title = format!("{who}Allow {tool}{what}?{reason_suffix}");
                let theme = s.theme.read().unwrap().clone();
                let preview = match t.get::<Value>("preview")? {
                    Value::Nil => None,
                    v => node_from(&theme, v).ok(),
                };
                let mut choices = Vec::new();
                let mut items = Vec::new();
                let mut keys: Vec<(char, String)> = Vec::new();
                let mut by_key: HashMap<char, String> = HashMap::new();
                for choice in raw_choices {
                    let (label, key_char, key_label) = match choice.as_str() {
                        "allow" => (
                            if scoped { "Allow requested access for this turn (y)" } else { "Allow once (y)" }
                                .to_string(),
                            Some('y'),
                            if scoped { "this turn" } else { "allow once" }.to_string(),
                        ),
                        "allow_session" => (
                            if scoped {
                                "Allow requested access for this session (a)"
                            } else {
                                "Allow for this session (a)"
                            }
                            .to_string(),
                            Some('a'),
                            "this session".to_string(),
                        ),
                        "allow_amend" => (
                            format!("Allow and remember (A): {}", amendment_desc.as_deref().unwrap_or("this rule")),
                            Some('A'),
                            "remember".to_string(),
                        ),
                        "deny" => ("Deny, and tell the model why (n)".to_string(), Some('n'), "deny".to_string()),
                        "abort" => ("Deny and stop this turn (ctrl+c)".to_string(), None, "deny and stop".to_string()),
                        other => return Err(mlua::Error::runtime(format!("invalid approval choice: {other}"))),
                    };
                    choices.push(choice.clone());
                    items.push(SelectItem { label, preview: preview.clone(), ..Default::default() });
                    if let Some(key_char) = key_char {
                        keys.push((key_char, key_label));
                        by_key.insert(key_char, choice);
                    }
                }
                // Ctrl+C is reserved by the core and interrupts the modal before key dispatch.
                keys.push(('\u{3}', "interrupt".to_string()));
                let cancel = parse_cancel(&t)?;
                if !s.can_respond() || cancel.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
                    return Ok((None::<String>, None::<String>));
                }
                let modal_id = s.next_id.fetch_add(1, Ordering::Relaxed);
                let (tx, rx) = oneshot::channel();
                let cmd = UiCmd::Select {
                    title,
                    title_right: vec![],
                    split: Default::default(),
                    items,
                    selected: 0,
                    multi: false,
                    accept_label: Some("confirm".into()),
                    allow_search: false,
                    filter: String::new(),
                    tabs: vec![],
                    tab: 0,
                    kind: "list".into(),
                    keys,
                    body: vec![],
                    vim: false,
                    highlight: None,
                    reply: tx,
                    token: 0,
                    modal_id,
                    approval: true,
                };
                if !s.send(cmd) {
                    return Ok((None::<String>, None::<String>));
                }
                let reply = match wait_select(&s, modal_id, rx, None, None, cancel.as_ref()).await {
                    Ok(Ok(reply)) => reply,
                    Ok(Err(Dismissed::Interrupted)) => return Ok((Some("abort".to_string()), None)),
                    Ok(Err(Dismissed::Cancelled)) | Err(()) => return Ok((None, None)),
                };
                let decision = if let Some(k) = reply.key {
                    by_key.get(&k).cloned().unwrap_or_else(|| "deny".into())
                } else {
                    choices.get(reply.index).cloned().unwrap_or_else(|| "deny".into())
                };
                let human_approval = if matches!(decision.as_str(), "allow" | "allow_session" | "allow_amend") {
                    Some(crate::approval::issue(&lua, binding, arguments, &decision))
                } else {
                    None
                };
                Ok((Some(decision), human_approval))
            }
        })?,
    )?;
    // input{title, placeholder, default, secret, cancel} -> string | nil
    let s = sh!();
    ui.set(
        "input",
        lua.create_async_function(move |_, t: Table| {
            let s = s.clone();
            async move { run_input_modal(&s, t, false).await }
        })?,
    )?;
    // editor{title, prefill, cancel} -> string | nil
    let s = sh!();
    ui.set(
        "editor",
        lua.create_async_function(move |_, t: Table| {
            let s = s.clone();
            async move { run_input_modal(&s, t, true).await }
        })?,
    )?;
    // questions{title, questions, cancel} -> result | nil, err
    let s = sh!();
    ui.set(
        "questions",
        lua.create_async_function(move |lua, t: Table| {
            let s = s.clone();
            async move { crate::ui_questions::run_questions(&lua, &s, t).await }
        })?,
    )?;
    Ok(())
}

pub(crate) fn parse_cancel(t: &Table) -> mlua::Result<Option<tokio_util::sync::CancellationToken>> {
    let Some(ud) = t.get::<Option<mlua::AnyUserData>>("cancel")? else {
        return Ok(None);
    };
    Ok(Some(ud.borrow::<crate::task::Token>()?.0.clone()))
}

struct ModalCancelGuard {
    ui: Arc<UiShared>,
    modal_id: u64,
    armed: bool,
    open: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Drop for ModalCancelGuard {
    fn drop(&mut self) {
        if let Some(open) = &self.open {
            open.store(false, Ordering::SeqCst);
        }
        if self.armed && self.modal_id != 0 {
            self.ui.send(UiCmd::CancelModal { modal_id: self.modal_id });
        }
    }
}

async fn cancel_or_pending(cancel: Option<&tokio_util::sync::CancellationToken>) {
    match cancel {
        Some(c) => c.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// Wait for a `Select` reply, supporting `on_highlight` updates and `cancel` token.
/// Returns `Err(())` if cancelled by `cancel`, `Ok(reply)` otherwise.
pub(crate) async fn wait_select(
    s: &Arc<UiShared>,
    modal_id: u64,
    rx: oneshot::Receiver<Result<mare_shared::ui::SelectReply, Dismissed>>,
    on_highlight: Option<&mlua::Function>,
    hrx: Option<tokio::sync::mpsc::UnboundedReceiver<(usize, usize, u64)>>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<Result<mare_shared::ui::SelectReply, Dismissed>, ()> {
    let mut guard = ModalCancelGuard { ui: s.clone(), modal_id, armed: true, open: None };
    wait_select_with_guard(s, &mut guard, rx, on_highlight, hrx, cancel).await
}

async fn wait_select_with_guard(
    s: &Arc<UiShared>,
    guard: &mut ModalCancelGuard,
    mut rx: oneshot::Receiver<Result<mare_shared::ui::SelectReply, Dismissed>>,
    on_highlight: Option<&mlua::Function>,
    mut hrx: Option<tokio::sync::mpsc::UnboundedReceiver<(usize, usize, u64)>>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<Result<mare_shared::ui::SelectReply, Dismissed>, ()> {
    loop {
        tokio::select! {
            r = &mut rx => {
                guard.armed = false;
                break Ok(r.unwrap_or(Err(Dismissed::Cancelled)));
            }
            _ = cancel_or_pending(cancel) => {
                break Err(());
            }
            Some((tab, index, revision)) = async {
                match hrx.as_mut() {
                    Some(ch) => ch.recv().await,
                    None => std::future::pending().await,
                }
            }, if on_highlight.is_some() => {
                let (mut tab, mut index, mut revision) = (tab, index, revision);
                if let Some(ch) = hrx.as_mut() {
                    while let Ok((t2, i2, r2)) = ch.try_recv() {
                        (tab, index, revision) = (t2, i2, r2);
                    }
                }
                let f = on_highlight.expect("checked");
                let hl_fut = f.call_async::<Value>((index + 1, tab + 1));
                tokio::select! {
                    r = &mut rx => {
                        guard.armed = false;
                        break Ok(r.unwrap_or(Err(Dismissed::Cancelled)));
                    }
                    _ = cancel_or_pending(cancel) => {
                        break Err(());
                    }
                    hl_res = hl_fut => {
                        if let Ok(v) = hl_res
                            && !v.is_nil()
                            && let Ok(node) = node_from(&s.theme.read().unwrap().clone(), v)
                        {
                            s.send(UiCmd::SelectPreview { modal_id: guard.modal_id, revision, tab, index, node });
                        }
                    }
                }
            }
        }
    }
}

/// Wait for an `Input` reply with `cancel` token support.
/// Returns `Err(())` if cancelled by `cancel`, `Ok(text)` otherwise.
pub(crate) async fn wait_input(
    s: &Arc<UiShared>,
    modal_id: u64,
    mut rx: oneshot::Receiver<Result<String, Dismissed>>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<Result<String, Dismissed>, ()> {
    let mut guard = ModalCancelGuard { ui: s.clone(), modal_id, armed: true, open: None };
    let res = tokio::select! {
        r = &mut rx => {
            guard.armed = false;
            Ok(r.unwrap_or(Err(Dismissed::Cancelled)))
        }
        _ = cancel_or_pending(cancel) => Err(()),
    };
    drop(guard);
    res
}

async fn run_input_modal(
    s: &Arc<UiShared>,
    t: Table,
    is_editor: bool,
) -> mlua::Result<(Option<String>, Option<String>)> {
    let cancel = parse_cancel(&t)?;
    if !s.can_respond() {
        return Ok((None, Some("unavailable".to_string())));
    }
    if cancel.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        return Ok((None, Some("cancelled".to_string())));
    }
    let modal_id = s.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    let default = if is_editor {
        t.get::<Option<String>>("prefill")?.unwrap_or_default()
    } else {
        t.get::<Option<String>>("default")?.unwrap_or_default()
    };
    let placeholder = t.get::<Option<String>>("placeholder")?.unwrap_or_default();
    let secret = if is_editor { false } else { t.get::<Option<bool>>("secret")?.unwrap_or(false) };
    let ok = s.send(UiCmd::Input {
        title: t.get::<Option<String>>("title")?.unwrap_or_default(),
        placeholder,
        default,
        secret,
        reply: tx,
        modal_id,
    });
    if !ok {
        return Ok((None, Some("unavailable".to_string())));
    }
    Ok(match wait_input(s, modal_id, rx, cancel.as_ref()).await {
        Ok(Ok(text)) => (Some(text), None),
        Ok(Err(reason)) => (None, Some(reason.as_str().to_string())),
        Err(()) => (None, Some("cancelled".to_string())),
    })
}
