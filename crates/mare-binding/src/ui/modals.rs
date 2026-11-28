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
                let amendment_desc = snapshot.amendment_description.clone();
                let is_subagent = snapshot.origin.is_some();
                let has_command = non_empty_str(&snapshot.arguments, "command").is_some();
                let has_reusable_sandbox_scope = reusable_sandbox_scope(&snapshot.arguments).is_some();
                let theme = s.theme.read().unwrap().clone();
                let preview = match t.get::<Value>("preview")? {
                    Value::Nil => None,
                    v => node_from(&theme, v).ok(),
                };
                let (title, body) = build_approval_presentation(&snapshot, &raw_choices, preview.is_some());
                let arguments = snapshot.arguments;
                let mut choices = Vec::new();
                let mut items = Vec::new();
                let mut keys: Vec<(char, String)> = Vec::new();
                let mut by_key: HashMap<char, String> = HashMap::new();
                for choice in raw_choices {
                    let (label, detail, key_char, key_label) = match choice.as_str() {
                        "allow" => (
                            if scoped { "Allow requested access for this turn (y)" } else { "Allow once (y)" }
                                .to_string(),
                            if scoped {
                                Some("reused by later commands until this turn ends".to_string())
                            } else {
                                None
                            },
                            Some('y'),
                            if scoped { "this turn" } else { "allow once" }.to_string(),
                        ),
                        "allow_session" => (
                            if scoped {
                                if is_subagent {
                                    "Allow requested access for this subagent session (a)"
                                } else {
                                    "Allow requested access for this session (a)"
                                }
                            } else if has_reusable_sandbox_scope {
                                "Allow command & requested access for this session (a)"
                            } else if has_command {
                                "Allow this exact command for this session (a)"
                            } else {
                                "Allow this exact call for this session (a)"
                            }
                            .to_string(),
                            if scoped {
                                Some("reused by later commands until session ends or permissions change".to_string())
                            } else {
                                None
                            },
                            Some('a'),
                            "this session".to_string(),
                        ),
                        "allow_amend" => (
                            format!("Allow and remember (A): {}", amendment_desc.as_deref().unwrap_or("this rule")),
                            None,
                            Some('A'),
                            "remember".to_string(),
                        ),
                        "deny" => (
                            if is_subagent {
                                "Deny, and tell the model why (n) — affects only this subagent"
                            } else {
                                "Deny, and tell the model why (n)"
                            }
                            .to_string(),
                            None,
                            Some('n'),
                            "deny".to_string(),
                        ),
                        "abort" => (
                            if is_subagent {
                                "Deny and stop this turn (ctrl+c) — stops only this subagent"
                            } else {
                                "Deny and stop this turn (ctrl+c)"
                            }
                            .to_string(),
                            None,
                            None,
                            "deny and stop".to_string(),
                        ),
                        other => return Err(mlua::Error::runtime(format!("invalid approval choice: {other}"))),
                    };
                    choices.push(choice.clone());
                    items.push(SelectItem { label, detail, preview: preview.clone(), ..Default::default() });
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
                    split: if has_reusable_sandbox_scope {
                        mare_shared::ui::SelectSplit { ratio: 0.52, min: 57 }
                    } else {
                        mare_shared::ui::SelectSplit { ratio: 0.46, min: 50 }
                    },
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
                    body,
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

fn non_empty_str<'a>(obj: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(serde_json::Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// Format subagent origin into `(primary_name, requester_detail)` without repeating identical
/// label/phase/task segments (e.g. when a subagent has only a description).
fn format_origin(origin: &serde_json::Value) -> (String, String) {
    let primary = non_empty_str(origin, "label")
        .or_else(|| non_empty_str(origin, "name"))
        .or_else(|| non_empty_str(origin, "agent_id"))
        .or_else(|| non_empty_str(origin, "subagent_id"))
        .unwrap_or("sub-agent")
        .to_string();
    let mut parts = vec![primary.clone()];
    let mut push_unique = |part: String| {
        if !part.is_empty() && !parts.iter().any(|existing| existing == &part) {
            parts.push(part);
        }
    };
    if let Some(run_id) = non_empty_str(origin, "run_id") {
        push_unique(format!("run {run_id}"));
    }
    if let Some(phase) = non_empty_str(origin, "phase") {
        push_unique(phase.to_string());
    }
    if let Some(task) = non_empty_str(origin, "task") {
        push_unique(task.to_string());
    }
    let mut detail = parts.join(" · ");
    if let Some(agent_id) = non_empty_str(origin, "agent_id")
        && !parts.iter().any(|p| p == agent_id)
    {
        use std::fmt::Write as _;
        let _ = write!(detail, " (id: {agent_id})");
    }
    (primary, detail)
}

fn format_approval_duration(snapshot: &crate::approval::Snapshot, raw_choices: &[String]) -> String {
    let has = |name: &str| raw_choices.iter().any(|c| c == name);
    let session_label = if snapshot.origin.is_some() {
        "this subagent session (a, until subagent ends or until the permission preset changes)"
    } else {
        "this session (a, until session ends or until the permission preset changes)"
    };
    let mut scopes = Vec::new();
    if has("allow") {
        scopes.push(if snapshot.scoped { "this turn (y, expires on turn end)" } else { "once (y, this call)" });
    }
    if has("allow_session") {
        scopes.push(session_label);
    }
    if has("allow_amend") {
        scopes.push("remember rule (A)");
    }
    if scopes.is_empty() {
        return "none".to_string();
    }
    if snapshot.protected_arguments {
        return format!("{} (protected arguments: once only)", scopes.join(" · "));
    }
    scopes.join(" · ")
}

/// Match `permissions/vote.lua`'s `source_tag() .. label_of(call, repr)` form (`[who] tool path`,
/// `tool path`, `[who] <command>`, or `<command>`) so policy warnings that happen to mention the
/// path or command (e.g. `credential path reminder: write <path>`) are never stripped.
fn is_redundant_label_segment(seg: &str, tool: &str, shown_path: Option<&str>, shown_cmd: Option<&str>) -> bool {
    let rest = if let Some(after_open) = seg.strip_prefix('[') {
        let Some((_, after_close)) = after_open.split_once("] ") else {
            return false;
        };
        after_close
    } else {
        seg
    };
    if let Some(path) = shown_path
        && rest.strip_prefix(tool).and_then(|s| s.strip_prefix(' ')) == Some(path)
    {
        return true;
    }
    if let Some(cmd) = shown_cmd
        && (rest == cmd || rest.strip_prefix(tool).and_then(|s| s.strip_prefix(' ')) == Some(cmd))
    {
        return true;
    }
    false
}

/// Clean up `permissions/vote.lua`'s composite reason (`"[who] tool <path> · write · cwd <dir> · …"`):
/// drop only segments already rendered on dedicated lines (`[who] tool <path>`, `[who] <cmd>`, bare
/// `tool`, `requests …`, `because: <justification>`, `maximum grant scope: …`, `cwd <dir>`), and
/// preserve all remaining policy explanations and warnings (credential reminders, dangerous
/// commands, prompt rules, `(via call_tool)`). Never elevates untrusted text into `Directory:`.
#[allow(clippy::too_many_arguments)]
fn split_reason_parts(
    reason: &str,
    tool: &str,
    shown_path: Option<&str>,
    shown_cmd: Option<&str>,
    has_cwd: bool,
    has_structured_permissions: bool,
    scoped: bool,
    justification: Option<&str>,
) -> Option<String> {
    let (reason_body, via_call_tool) = match reason.strip_suffix(" (via call_tool)") {
        Some(rest) => (rest, true),
        None => (reason, false),
    };
    let mut kept: Vec<&str> = Vec::new();
    for (idx, seg) in reason_body.split(" · ").map(str::trim).filter(|s| !s.is_empty()).enumerate() {
        if idx == 0 && is_redundant_label_segment(seg, tool, shown_path, shown_cmd) {
            continue;
        }
        if has_cwd && !has_structured_permissions && seg.starts_with("cwd ") {
            continue;
        }
        if seg == tool && shown_path.is_some() {
            continue;
        }
        if has_structured_permissions && seg.starts_with("requests ") {
            continue;
        }
        if scoped && seg.starts_with("maximum grant scope:") {
            continue;
        }
        if justification.is_some_and(|j| seg.strip_prefix("because: ") == Some(j)) {
            continue;
        }
        kept.push(seg);
    }
    if via_call_tool {
        kept.push("(via call_tool)");
    }
    (!kept.is_empty()).then(|| kept.join(" · "))
}

/// A non-`all` `sandbox_permissions` escalation on a shell tool is merged into
/// `P.approved.session` on `allow_session` and reused by subsequent commands, unlike exact-call
/// session caching (see `permissions/approvals.lua`).
fn reusable_sandbox_scope(arguments: &serde_json::Value) -> Option<&str> {
    non_empty_str(arguments, "sandbox_permissions").filter(|s| !matches!(*s, "all" | "danger-full-access"))
}

fn title_first_line(text: &str, max_chars: usize) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    if first.chars().count() <= max_chars {
        first.to_string()
    } else {
        let mut s: String = first.chars().take(max_chars.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

fn format_generic_arguments(arguments: &serde_json::Value, exclude_keys: &[&str]) -> Option<String> {
    let filtered = match arguments {
        serde_json::Value::Null => return None,
        serde_json::Value::Object(m) => {
            let kept: serde_json::Map<String, serde_json::Value> = m
                .iter()
                .filter(|(k, _)| !exclude_keys.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if kept.is_empty() {
                return None;
            }
            serde_json::Value::Object(kept)
        }
        serde_json::Value::Array(a) if a.is_empty() => return None,
        serde_json::Value::String(s) if s.trim().is_empty() => return None,
        other => other.clone(),
    };
    let raw = serde_json::to_string(&filtered).ok()?;
    const MAX_ARG_CHARS: usize = 2048;
    if raw.chars().count() <= MAX_ARG_CHARS {
        Some(raw)
    } else {
        let mut s: String = raw.chars().take(MAX_ARG_CHARS).collect();
        use std::fmt::Write as _;
        let _ = write!(s, "… ({} bytes total)", raw.len());
        Some(s)
    }
}

/// Build the native approval dialog border title and structured, non-truncated detail body from
/// the immutable approval snapshot (compact title + full wrapped detail lines).
fn build_approval_presentation(
    snapshot: &crate::approval::Snapshot,
    raw_choices: &[String],
    has_preview: bool,
) -> (String, Vec<String>) {
    let mut body = Vec::new();
    let shown_cmd = non_empty_str(&snapshot.arguments, "command");
    let title_subject = match shown_cmd {
        Some(c) => format!("{}: {}", snapshot.tool, title_first_line(c, 60)),
        None => snapshot.tool.clone(),
    };
    let title = if let Some(origin) = snapshot.origin.as_ref() {
        let (primary, requester_detail) = format_origin(origin);
        body.push(format!("Requester: Subagent {requester_detail}"));
        format!("Subagent · {primary} — Allow {title_subject}?")
    } else {
        body.push("Requester: Main session".to_string());
        format!("Allow {title_subject}?")
    };

    let tool_line = match snapshot.call_id.as_deref() {
        Some(id) => format!("Tool: {} (call {id})", snapshot.tool),
        None => format!("Tool: {}", snapshot.tool),
    };
    body.push(tool_line);

    let mut showed_specific_args = false;
    let shown_path =
        non_empty_str(&snapshot.arguments, "path").or_else(|| non_empty_str(&snapshot.arguments, "file_path"));
    if let Some(path) = shown_path {
        showed_specific_args = true;
        body.push(format!("Path: {path}"));
    }
    let shown_cwd = snapshot.cwd.as_deref().filter(|s| !s.is_empty());
    let shell_preview = has_preview && matches!(snapshot.tool.as_str(), "bash" | "powershell");
    if let Some(cwd) = shown_cwd
        && !shell_preview
    {
        body.push(format!("Directory: {cwd}"));
    }
    if let Some(cmd) = shown_cmd {
        showed_specific_args = true;
        if !has_preview {
            let mut lines = cmd.lines();
            if let Some(first) = lines.next() {
                body.push(format!("Command: {first}"));
                for cont in lines {
                    body.push(format!("  {cont}"));
                }
            }
        }
    }
    if let Some(scope) = reusable_sandbox_scope(&snapshot.arguments) {
        body.push(format!("Requested access: {scope}"));
    }
    if snapshot.tool == "web_fetch"
        && let Some(url) = non_empty_str(&snapshot.arguments, "url")
    {
        body.push(format!("URL: {url}"));
    }
    let mut has_structured_permissions = false;
    let perms = snapshot
        .requested_permissions
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .or_else(|| snapshot.arguments.get("additional_permissions").and_then(serde_json::Value::as_object));
    if let Some(additional) = perms {
        for (field, heading) in [("read", "Read paths:"), ("write", "Write paths:")] {
            if let Some(paths) = additional.get(field).and_then(serde_json::Value::as_array)
                && !paths.is_empty()
            {
                has_structured_permissions = true;
                body.push(heading.to_string());
                for item in paths {
                    if let Some(p) = item.as_str().filter(|s| !s.is_empty()) {
                        body.push(format!("  - {p}"));
                    }
                }
            }
        }
        match additional.get("network") {
            Some(serde_json::Value::Bool(true)) => {
                has_structured_permissions = true;
                body.push("Network: all hosts".to_string());
            }
            Some(serde_json::Value::Array(hosts)) if !hosts.is_empty() => {
                let list: Vec<&str> = hosts.iter().filter_map(serde_json::Value::as_str).collect();
                if !list.is_empty() {
                    has_structured_permissions = true;
                    body.push(format!("Network: {}", list.join(", ")));
                }
            }
            _ => {}
        }
    }
    if !showed_specific_args {
        let mut exclude = vec!["additional_permissions", "justification", "sandbox_permissions"];
        if snapshot.tool == "web_fetch" {
            exclude.push("url");
        }
        if snapshot.scoped {
            exclude.push("scope");
        }
        if let Some(arg_summary) = format_generic_arguments(&snapshot.arguments, &exclude) {
            body.push(format!("Arguments: {arg_summary}"));
        }
    }

    let justification = non_empty_str(&snapshot.arguments, "justification");
    let reason = snapshot.reason.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(j) = justification {
        body.push(format!("Purpose: {j}"));
    }
    if let Some(r) = reason
        && let Some(clean) = split_reason_parts(
            r,
            &snapshot.tool,
            shown_path,
            shown_cmd,
            shown_cwd.is_some(),
            has_structured_permissions,
            snapshot.scoped,
            justification,
        )
    {
        body.push(format!("Reason: {clean}"));
    }
    if snapshot.unconfined {
        body.push("Sandbox: runs outside the local sandbox".to_string());
    }
    if snapshot.protected_arguments {
        body.push("Protected: hidden registered values restored at execution (approve once)".to_string());
    }
    body.push(format!("Scope: {}", format_approval_duration(snapshot, raw_choices)));

    (title, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::Snapshot;

    #[test]
    fn subagent_approval_deduplicates_origin_and_preserves_full_long_paths() {
        let long_path = format!("/workspace/{}/target_permission_selector.rs", "nested_segment/".repeat(12));
        let snap = Snapshot {
            tool: "write".into(),
            call_id: Some("call_sub_42".into()),
            cwd: Some("/workspace".into()),
            arguments: serde_json::json!({
                "path": long_path,
                "justification": "update selector layout",
            }),
            requested_permissions: None,
            unconfined: false,
            protected_arguments: false,
            origin: Some(serde_json::json!({
                "agent_id": "agent-3",
                "label": "测试权限选择器",
                "task": "测试权限选择器",
            })),
            scoped: false,
            reason: Some(format!("[测试权限选择器] write {long_path} · write · cwd /workspace")),
            amendment_description: None,
        };
        let choices = vec!["allow".into(), "allow_session".into(), "deny".into(), "abort".into()];
        let (title, body) = build_approval_presentation(&snap, &choices, false);
        assert_eq!(title, "Subagent · 测试权限选择器 — Allow write?");
        let joined = body.join("\n");
        assert!(
            !joined.contains("测试权限选择器 · 测试权限选择器"),
            "identical label and task must not duplicate: {joined}"
        );
        assert!(joined.contains("Requester: Subagent 测试权限选择器 (id: agent-3)"), "{joined}");
        assert!(joined.contains("Tool: write (call call_sub_42)"), "{joined}");
        assert!(joined.contains(&format!("Path: {long_path}")), "full path must not be truncated: {joined}");
        assert!(joined.contains("Directory: /workspace"), "cwd read from frozen call.cwd: {joined}");
        assert!(!joined.contains("[测试权限选择器] write"), "requester/path must not repeat in Reason: {joined}");
        assert!(!joined.contains("Reason: write"), "bare tool name must not repeat in Reason: {joined}");
        assert!(joined.contains("Purpose: update selector layout"), "{joined}");
        assert!(
            joined.contains(
                "Scope: once (y, this call) · this subagent session (a, until subagent ends or until the permission preset changes)"
            ),
            "{joined}"
        );
    }

    #[test]
    fn request_permissions_lists_all_full_paths_and_turn_only_scope() {
        let long_dir = format!("/home/user/projects/{}/output ", "deep_component/".repeat(10));
        let snap = Snapshot {
            tool: "request_permissions".into(),
            call_id: Some("call_perm_9".into()),
            cwd: None,
            arguments: serde_json::json!({
                "additional_permissions": {
                    "write": ["relative/raw/dir"],
                    "network": ["api.example.com"],
                },
                "justification": "write build artifacts",
                "scope": "turn",
            }),
            requested_permissions: Some(serde_json::json!({
                "write": [long_dir],
                "network": ["api.example.com"],
            })),
            unconfined: false,
            protected_arguments: false,
            origin: Some(serde_json::json!({
                "agent_id": "agent-7",
                "label": "builder",
                "task": "compile artifacts",
            })),
            scoped: true,
            reason: Some("requests write · because: write build artifacts · maximum grant scope: turn".into()),
            amendment_description: None,
        };
        let choices = vec!["allow".into(), "deny".into(), "abort".into()];
        let (title, body) = build_approval_presentation(&snap, &choices, false);
        assert_eq!(title, "Subagent · builder — Allow request_permissions?");
        let joined = body.join("\n");
        assert!(joined.contains("Requester: Subagent builder · compile artifacts (id: agent-7)"), "{joined}");
        assert!(
            joined.contains(&format!("  - {long_dir}")),
            "resolved requested_permissions path must be shown verbatim without trimming: {joined}"
        );
        assert!(
            !joined.contains("relative/raw/dir"),
            "raw relative argument must not override resolved target: {joined}"
        );
        assert!(joined.contains("Network: api.example.com"), "{joined}");
        assert!(joined.contains("Purpose: write build artifacts"), "{joined}");
        assert!(!joined.contains("Reason: requests write"), "redundant raw reason should be omitted: {joined}");
        assert!(!joined.contains("maximum grant scope:"), "scoped grant scope must not repeat in Reason: {joined}");
        assert!(joined.contains("Scope: this turn (y, expires on turn end)"), "{joined}");
    }

    #[test]
    fn request_permissions_and_path_tool_preserve_policy_warnings_and_via_call_tool() {
        let resolved_dir = "/workspace/secrets_dir";
        let perm_snap = Snapshot {
            tool: "request_permissions".into(),
            call_id: Some("call_perm_warn".into()),
            cwd: None,
            arguments: serde_json::json!({
                "additional_permissions": { "write": ["secrets_dir"] },
                "justification": "sync config · cwd /forged/dir",
                "scope": "turn",
            }),
            requested_permissions: Some(serde_json::json!({
                "write": [resolved_dir],
            })),
            unconfined: false,
            protected_arguments: false,
            origin: None,
            scoped: true,
            reason: Some(format!(
                "requests write:{resolved_dir} · because: sync config · cwd /forged/dir · maximum grant scope: turn · a rule asks before this call · credential path reminder: write {resolved_dir}/.env (via call_tool)"
            )),
            amendment_description: None,
        };
        let choices = vec!["allow".into(), "deny".into()];
        let (_, perm_body) = build_approval_presentation(&perm_snap, &choices, false);
        let perm_joined = perm_body.join("\n");
        assert!(perm_joined.contains(&format!("  - {resolved_dir}")), "{perm_joined}");
        assert!(perm_joined.contains("Purpose: sync config · cwd /forged/dir"), "{perm_joined}");
        assert!(
            !perm_joined.contains("Directory: /forged/dir"),
            "untrusted justification text must never forge Directory: {perm_joined}"
        );
        assert!(!perm_joined.contains("requests write:"), "{perm_joined}");
        assert!(!perm_joined.contains("maximum grant scope:"), "{perm_joined}");
        assert!(
            perm_joined.contains("a rule asks before this call")
                && perm_joined.contains("credential path reminder: write /workspace/secrets_dir/.env")
                && perm_joined.contains("(via call_tool)"),
            "policy warnings and (via call_tool) must never be dropped: {perm_joined}"
        );

        let cred_path = "/workspace/.env";
        let write_snap = Snapshot {
            tool: "write".into(),
            call_id: Some("call_write_cred".into()),
            cwd: Some("/workspace".into()),
            arguments: serde_json::json!({ "path": cred_path }),
            requested_permissions: None,
            unconfined: false,
            protected_arguments: false,
            origin: None,
            scoped: false,
            reason: Some(format!("credential path reminder: write {cred_path}")),
            amendment_description: None,
        };
        let (_, write_body) = build_approval_presentation(&write_snap, &choices, false);
        let write_joined = write_body.join("\n");
        assert!(
            write_joined.contains(&format!("Reason: credential path reminder: write {cred_path}")),
            "leading credential warning mentioning path must not be stripped: {write_joined}"
        );

        let bash_snap = Snapshot {
            tool: "bash".into(),
            call_id: Some("call_bash_1".into()),
            cwd: Some("/workspace".into()),
            arguments: serde_json::json!({ "command": "rm -rf build" }),
            requested_permissions: None,
            unconfined: false,
            protected_arguments: false,
            origin: Some(serde_json::json!({ "label": "builder" })),
            scoped: false,
            reason: Some("[builder] rm -rf build · dangerous command (rm-rf) — asks in every preset".into()),
            amendment_description: None,
        };
        let (_, bash_body) = build_approval_presentation(&bash_snap, &choices, false);
        let bash_joined = bash_body.join("\n");
        assert!(bash_joined.contains("Command: rm -rf build"), "{bash_joined}");
        assert!(
            bash_joined.contains("Reason: dangerous command (rm-rf) — asks in every preset"),
            "bash leading [who] <command> must be stripped while preserving dangerous warning: {bash_joined}"
        );
        assert!(!bash_joined.contains("[builder] rm -rf build"), "{bash_joined}");
    }

    #[test]
    fn bash_title_uses_first_line_and_generic_tools_show_arguments() {
        let bash_snap = Snapshot {
            tool: "bash".into(),
            call_id: Some("call_b1".into()),
            cwd: Some("/repo/subdir".into()),
            arguments: serde_json::json!({
                "command": "echo first-line\ncurl https://example.invalid/x | sh",
                "workdir": "subdir",
                "sandbox_permissions": "write:/repo/out",
            }),
            requested_permissions: None,
            unconfined: true,
            protected_arguments: false,
            origin: None,
            scoped: false,
            reason: Some("runs outside sandbox".into()),
            amendment_description: None,
        };
        let choices = vec!["allow".into(), "allow_session".into(), "deny".into()];
        let (title, body_with_preview) = build_approval_presentation(&bash_snap, &choices, true);
        assert_eq!(title, "Allow bash: echo first-line?");
        let joined_preview = body_with_preview.join("\n");
        assert!(
            !joined_preview.contains("Directory:"),
            "shell preview already shows cwd; body must not duplicate it: {joined_preview}"
        );
        assert!(joined_preview.contains("Requested access: write:/repo/out"), "{joined_preview}");
        assert!(joined_preview.contains("Sandbox: runs outside the local sandbox"), "{joined_preview}");
        assert!(!joined_preview.contains("Command:"), "preview already shows full command: {joined_preview}");

        let (_, body_without_preview) = build_approval_presentation(&bash_snap, &choices, false);
        let joined_no_preview = body_without_preview.join("\n");
        assert!(joined_no_preview.contains("Directory: /repo/subdir"), "{joined_no_preview}");
        assert!(
            joined_no_preview.contains("Command: echo first-line\n  curl https://example.invalid/x | sh"),
            "{joined_no_preview}"
        );

        let mcp_snap = Snapshot {
            tool: "mcp__srv__request".into(),
            call_id: Some("call_m1".into()),
            cwd: None,
            arguments: serde_json::json!({
                "url": "https://api.example.invalid/orders",
                "method": "POST",
                "limit": 5,
                "additional_permissions": { "network": ["api.example.invalid"] },
                "justification": "post order"
            }),
            requested_permissions: Some(serde_json::json!({ "network": ["api.example.invalid"] })),
            unconfined: false,
            protected_arguments: false,
            origin: None,
            scoped: false,
            reason: None,
            amendment_description: None,
        };
        let (mcp_title, mcp_body) = build_approval_presentation(&mcp_snap, &choices, false);
        assert_eq!(mcp_title, "Allow mcp__srv__request?");
        let mcp_joined = mcp_body.join("\n");
        assert!(
            !mcp_joined.contains("URL:"),
            "non-web_fetch MCP tool must not duplicate url as a separate URL: line: {mcp_joined}"
        );
        assert!(mcp_joined.contains("Network: api.example.invalid"), "{mcp_joined}");
        assert!(
            mcp_joined.contains(r#"Arguments: {"url":"https://api.example.invalid/orders","method":"POST","limit":5}"#),
            "MCP tools with additional_permissions and url must still display business Arguments JSON: {mcp_joined}"
        );

        let fetch_snap = Snapshot {
            tool: "web_fetch".into(),
            call_id: Some("call_w1".into()),
            cwd: None,
            arguments: serde_json::json!({ "url": "https://example.invalid/doc" }),
            requested_permissions: None,
            unconfined: false,
            protected_arguments: false,
            origin: None,
            scoped: false,
            reason: None,
            amendment_description: None,
        };
        let (_, fetch_body) = build_approval_presentation(&fetch_snap, &choices, false);
        let fetch_joined = fetch_body.join("\n");
        assert!(fetch_joined.contains("URL: https://example.invalid/doc"), "{fetch_joined}");
        assert!(
            !fetch_joined.contains("Arguments:"),
            "web_fetch should not duplicate URL in Arguments: {fetch_joined}"
        );
    }
}
