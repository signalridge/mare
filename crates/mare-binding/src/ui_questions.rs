//! TUI implementation of `mare.ui.questions`: walks structured questions using
//! `UiCmd::Select` and `UiCmd::Input` modals, supporting single-select, multi-select,
//! custom text input, skip, Esc dismiss, and `cancel` token cancellation.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use mlua::{Lua, LuaSerdeExt, Table, Value};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use mare_shared::ui::{Dismissed, SelectItem, SelectReply, UiCmd, UiShared};

use crate::ui::{parse_cancel, render_node_text, wait_input, wait_select};

struct QuestionOpt {
    label: String,
    description: Option<String>,
    recommended: bool,
}

struct QuestionItem {
    id: String,
    header: Option<String>,
    question: String,
    options: Vec<QuestionOpt>,
    multi_select: bool,
    allow_custom: bool,
    allow_skip: bool,
    preview: Option<String>,
}

struct QuestionAnswer {
    selected: Vec<String>,
    custom: Option<String>,
    skipped: bool,
}

enum StepError {
    Cancelled,
    Dismissed(Dismissed),
    Unavailable,
}

enum StepOutcome {
    Answered(QuestionAnswer),
    Dismissed(Dismissed),
    Cancelled,
    Unavailable,
}

pub(crate) async fn run_questions(lua: &Lua, s: &Arc<UiShared>, t: Table) -> mlua::Result<(Value, Option<String>)> {
    if !s.can_respond() {
        return Ok((Value::Nil, Some("unavailable".to_string())));
    }
    let cancel = parse_cancel(&t)?;
    if cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
        return Ok((Value::Nil, Some("cancelled".to_string())));
    }
    let title: Option<String> = t.get("title")?;
    let questions = parse_questions(s, &t)?;
    let total = questions.len();

    let mut answers: HashMap<String, QuestionAnswer> = HashMap::new();
    let mut dismissed = None;

    for (idx, q) in questions.iter().enumerate() {
        let q_title = match (&title, &q.header) {
            (Some(t), Some(h)) if !t.is_empty() && !h.is_empty() => format!("{t} — {h}"),
            (_, Some(h)) if !h.is_empty() => h.clone(),
            (Some(t), _) if !t.is_empty() => t.clone(),
            _ => format!("Question {}/{}", idx + 1, total),
        };
        let mut body: Vec<String> = q.question.lines().map(str::to_string).collect();
        if let Some(preview) = &q.preview
            && !preview.is_empty()
        {
            body.push(String::new());
            body.extend(preview.lines().map(str::to_string));
        }

        let outcome = if q.options.is_empty() {
            let input_title = if q.question.is_empty() {
                q_title.clone()
            } else if q_title == q.question {
                q.question.clone()
            } else {
                format!("{q_title}: {}", q.question)
            };
            let placeholder = q.preview.as_deref().filter(|p| !p.is_empty()).unwrap_or("Enter answer...");
            ask_custom_input(s, &input_title, placeholder, &cancel).await
        } else if q.multi_select {
            ask_multi_select(s, &q_title, &body, q, &cancel).await
        } else {
            ask_single_select(s, &q_title, &body, q, &cancel).await
        };

        match outcome {
            StepOutcome::Answered(ans) => {
                answers.insert(q.id.clone(), ans);
            }
            StepOutcome::Dismissed(reason) => {
                dismissed = Some(reason);
                break;
            }
            StepOutcome::Cancelled => {
                return Ok((Value::Nil, Some("cancelled".to_string())));
            }
            StepOutcome::Unavailable => {
                return Ok((Value::Nil, Some("unavailable".to_string())));
            }
        }
    }

    let res = lua.create_table()?;
    let ans_tbl = lua.create_table()?;
    if dismissed.is_none() {
        for (id, ans) in answers {
            let entry = lua.create_table()?;
            let sel = lua.create_table()?;
            sel.set_metatable(Some(lua.array_metatable()))?;
            for (i, s) in ans.selected.into_iter().enumerate() {
                sel.set(i + 1, s)?;
            }
            entry.set("selected", sel)?;
            if let Some(c) = ans.custom {
                entry.set("custom", c)?;
            }
            if ans.skipped {
                entry.set("skipped", true)?;
            }
            ans_tbl.set(id, entry)?;
        }
    }
    res.set("answers", ans_tbl)?;
    if let Some(reason) = dismissed {
        res.set("dismissed", true)?;
        res.set("reason", reason.as_str())?;
    }
    Ok((Value::Table(res), None))
}

fn parse_questions(s: &UiShared, t: &Table) -> mlua::Result<Vec<QuestionItem>> {
    let mut questions = vec![];
    if let Some(list) = t.get::<Option<Table>>("questions")? {
        for qv in list.sequence_values::<Value>() {
            let Value::Table(qt) = qv? else { continue };
            let mut options = vec![];
            if let Some(opts) = qt.get::<Option<Table>>("options")? {
                for ov in opts.sequence_values::<Value>() {
                    let Value::Table(ot) = ov? else { continue };
                    options.push(QuestionOpt {
                        label: ot.get::<Option<String>>("label")?.unwrap_or_default(),
                        description: ot.get("description")?,
                        recommended: ot.get::<Option<bool>>("recommended")?.unwrap_or(false),
                    });
                }
            }
            let preview = match qt.get::<Value>("preview")? {
                Value::Nil => None,
                Value::String(st) => Some(st.to_str()?.to_string()),
                pv => Some(render_node_text(s, pv, 80)?),
            };
            questions.push(QuestionItem {
                id: qt.get::<Option<String>>("question_id")?.unwrap_or_default(),
                header: qt.get("header")?,
                question: qt.get::<Option<String>>("question")?.unwrap_or_default(),
                options,
                multi_select: qt.get::<Option<bool>>("multi_select")?.unwrap_or(false),
                allow_custom: qt.get::<Option<bool>>("allow_custom")?.unwrap_or(true),
                allow_skip: qt.get::<Option<bool>>("allow_skip")?.unwrap_or(true),
                preview,
            });
        }
    }
    Ok(questions)
}

fn option_label(opt: &QuestionOpt) -> String {
    if opt.recommended { format!("{} (recommended)", opt.label) } else { opt.label.clone() }
}

async fn send_select(
    s: &Arc<UiShared>,
    title: String,
    body: Vec<String>,
    items: Vec<SelectItem>,
    multi: bool,
    cancel: &Option<CancellationToken>,
) -> Result<SelectReply, StepError> {
    if !s.can_respond() {
        return Err(StepError::Unavailable);
    }
    if cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
        return Err(StepError::Cancelled);
    }
    let modal_id = s.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    let ok = s.send(UiCmd::Select {
        title,
        title_right: vec![],
        split: Default::default(),
        items,
        selected: 0,
        multi,
        accept_label: None,
        allow_search: true,
        filter: String::new(),
        tabs: vec![],
        tab: 0,
        kind: "list".into(),
        keys: vec![],
        body,
        vim: false,
        highlight: None,
        reply: tx,
        token: 0,
        modal_id,
        approval: false,
    });
    if !ok {
        return Err(StepError::Unavailable);
    }
    match wait_select(s, modal_id, rx, None, None, cancel.as_ref()).await {
        Err(()) => Err(StepError::Cancelled),
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(reason)) => Err(StepError::Dismissed(reason)),
    }
}

async fn ask_custom_input(
    s: &Arc<UiShared>,
    title: &str,
    placeholder: &str,
    cancel: &Option<CancellationToken>,
) -> StepOutcome {
    if !s.can_respond() {
        return StepOutcome::Unavailable;
    }
    if cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
        return StepOutcome::Cancelled;
    }
    let modal_id = s.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    let ok = s.send(UiCmd::Input {
        title: title.to_string(),
        placeholder: placeholder.to_string(),
        default: String::new(),
        secret: false,
        reply: tx,
        modal_id,
    });
    if !ok {
        return StepOutcome::Unavailable;
    }
    match wait_input(s, modal_id, rx, cancel.as_ref()).await {
        Ok(Ok(text)) => StepOutcome::Answered(QuestionAnswer { selected: vec![], custom: Some(text), skipped: false }),
        Ok(Err(reason)) => StepOutcome::Dismissed(reason),
        Err(()) => StepOutcome::Cancelled,
    }
}

async fn ask_single_select(
    s: &Arc<UiShared>,
    title: &str,
    body: &[String],
    q: &QuestionItem,
    cancel: &Option<CancellationToken>,
) -> StepOutcome {
    let mut items: Vec<SelectItem> = q
        .options
        .iter()
        .map(|o| SelectItem { label: option_label(o), detail: o.description.clone(), ..Default::default() })
        .collect();
    if q.allow_custom {
        items.push(SelectItem { label: "Other (custom answer)...".to_string(), ..Default::default() });
    }
    if q.allow_skip {
        items.push(SelectItem { label: "Skip".to_string(), ..Default::default() });
    }

    match send_select(s, title.to_string(), body.to_vec(), items, false, cancel).await {
        Err(StepError::Cancelled) => StepOutcome::Cancelled,
        Err(StepError::Unavailable) => StepOutcome::Unavailable,
        Err(StepError::Dismissed(reason)) => StepOutcome::Dismissed(reason),
        Ok(r) if r.index < q.options.len() => StepOutcome::Answered(QuestionAnswer {
            selected: vec![q.options[r.index].label.clone()],
            custom: None,
            skipped: false,
        }),
        Ok(r) if q.allow_custom && r.index == q.options.len() => {
            ask_custom_input(s, &q.question, "Enter custom answer...", cancel).await
        }
        Ok(_) => StepOutcome::Answered(QuestionAnswer { selected: vec![], custom: None, skipped: true }),
    }
}

async fn ask_multi_select(
    s: &Arc<UiShared>,
    title: &str,
    body: &[String],
    q: &QuestionItem,
    cancel: &Option<CancellationToken>,
) -> StepOutcome {
    let mut items: Vec<SelectItem> = q
        .options
        .iter()
        .map(|o| SelectItem { label: option_label(o), detail: o.description.clone(), ..Default::default() })
        .collect();
    let custom_index = q.allow_custom.then_some(items.len());
    if q.allow_custom {
        items.push(SelectItem { label: "Other (custom answer)...".to_string(), ..Default::default() });
    }
    let skip_index = q.allow_skip.then_some(items.len());
    if q.allow_skip {
        items.push(SelectItem { label: "Skip".to_string(), ..Default::default() });
    }
    match send_select(s, title.to_string(), body.to_vec(), items, true, cancel).await {
        Err(StepError::Cancelled) => StepOutcome::Cancelled,
        Err(StepError::Unavailable) => StepOutcome::Unavailable,
        Err(StepError::Dismissed(reason)) => StepOutcome::Dismissed(reason),
        Ok(reply) => {
            if skip_index.is_some_and(|i| reply.selected_indices.contains(&i)) {
                return StepOutcome::Answered(QuestionAnswer { selected: vec![], custom: None, skipped: true });
            }
            let selected =
                reply.selected_indices.iter().filter_map(|&i| q.options.get(i).map(|o| o.label.clone())).collect();
            if custom_index.is_some_and(|i| reply.selected_indices.contains(&i)) {
                match ask_custom_input(s, &q.question, "Enter custom answer...", cancel).await {
                    StepOutcome::Answered(mut answer) => {
                        answer.selected = selected;
                        StepOutcome::Answered(answer)
                    }
                    other => other,
                }
            } else {
                StepOutcome::Answered(QuestionAnswer { selected, custom: None, skipped: false })
            }
        }
    }
}
