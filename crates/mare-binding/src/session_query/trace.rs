//! Session lineage and event surface tracing over authorized history.
use mare_shared::lua::{json_to_lua, rt_err};
use mlua::{Lua, Table};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::corpus::{catalog, corpus_with_live, ensure_stamps_unchanged, overlay_headers, validate_live};
use super::{
    CancelOnDrop, Corpus, Position, ReadItem, TraceEventArgs, clip, decode, digest, encode, parse_live, scope, text,
};

async fn run_blocking<F>(f: F) -> mlua::Result<Value>
where
    F: FnOnce() -> mlua::Result<Value> + Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(rt_err)?
}

#[cfg(test)]
pub(super) fn trace_event_in_corpus(
    mut c: Corpus,
    target_session: &str,
    target_seq: usize,
    expected_gen: Option<&str>,
) -> mlua::Result<Value> {
    trace_event_in_corpus_projected(
        &mut c,
        target_session,
        target_seq,
        expected_gen,
        &crate::projection::JsonProjector::default(),
    )
}

pub(super) fn trace_event_in_corpus_projected(
    c: &mut Corpus,
    target_session: &str,
    target_seq: usize,
    expected_gen: Option<&str>,
    projector: &crate::projection::JsonProjector,
) -> mlua::Result<Value> {
    let entries = c
        .sessions
        .iter_mut()
        .find(|e| e[0]["session_id"] == target_session)
        .ok_or_else(|| super::query_err("QUERY_NOT_FOUND: session unavailable in this workspace"))?;
    let actual_gen = digest(entries);
    if let Some(exp) = expected_gen
        && exp != actual_gen
    {
        return Err(super::coded_err("stale_session", "stale history event; repeat search"));
    }
    if target_seq == 0 || target_seq >= entries.len() {
        return Err(super::query_err("QUERY_NOT_FOUND: event unavailable in this workspace"));
    }
    for entry in entries.iter_mut() {
        projector.project_json(entry);
    }
    let pos = Position { generation: actual_gen, session_id: target_session.to_owned(), seq: target_seq };
    let eid = encode(&pos);
    let mut traced =
        crate::session_surface::trace_event(entries, target_seq, &eid, &clip(&text(&entries[target_seq]), 512))?;
    traced["availability"] = json!(if c.live_ids.contains(target_session) { "live" } else { "persisted" });
    traced["incomplete"] = json!(c.incomplete);
    Ok(traced)
}

pub(super) fn trace_item_in_corpus_projected(
    c: &mut Corpus,
    session_id: &str,
    item_id: &str,
    projector: &crate::projection::JsonProjector,
) -> mlua::Result<Value> {
    if session_id.is_empty() || item_id.is_empty() {
        return Err(super::coded_err("invalid_arguments", "session_id and item_id are required"));
    }
    let entries = c
        .sessions
        .iter()
        .find(|e| e[0]["session_id"] == session_id)
        .ok_or_else(|| super::coded_err("unavailable", "session unavailable in this workspace"))?;
    let item = crate::session_surface::semantic_items(entries)
        .into_iter()
        .find(|item| item.value["item_id"] == item_id)
        .ok_or_else(|| super::coded_err("session.unknown_item", "item unavailable in this session"))?;
    let mut result = trace_event_in_corpus_projected(c, session_id, item.seq, None, projector)?;
    result.as_object_mut().expect("trace is an object").remove("event_id");
    result["item_id"] = json!(item_id);
    result["type"] = item.value["type"].clone();
    if let Some(result_id) = item.value.get("result_entry_id") {
        result["result_entry_id"] = result_id.clone();
    }
    Ok(result)
}

pub(super) fn install(lua: &Lua, store: &Table) -> mlua::Result<()> {
    store.set(
        "trace_session",
        lua.create_async_function(|lua, (args, opts): (Table, Option<Table>)| {
            let cancel = crate::task::opt_token(&opts).map_or_else(CancellationToken::new, |t| t.child_token());
            let guard = CancelOnDrop(cancel.clone());
            let prepared: mlua::Result<_> = (|| {
                let live = parse_live(&lua, &opts)?;
                let target: String = args.get("session_id")?;
                if target.is_empty() || target.len() > 256 {
                    return Err(super::query_err("QUERY_INVALID_FILTER: invalid session id"));
                }
                let source = scope(&lua)?;
                let projector = crate::projection::JsonProjector::from_lua(&lua)?;
                Ok((target, source, live, projector))
            })();
            async move {
                let _guard = guard;
                let (target, source, live, projector) = prepared?;
                let result = run_blocking(move || {
                    let view = catalog(&source, &cancel)?;
                    let live_records = validate_live(&source, &live, true);
                    let headers = overlay_headers(&view.headers, &live_records);
                    let mut result = crate::session_lineage::trace(&headers, &target, &cancel)?;
                    ensure_stamps_unchanged(&view.sources)?;
                    result["incomplete"] = json!(view.incomplete);
                    projector.project_json(&mut result);
                    Ok(result)
                })
                .await?;
                json_to_lua(&lua, &result)
            }
        })?,
    )?;

    store.set(
        "trace_event",
        lua.create_async_function(|lua, (args, opts): (Table, Option<Table>)| {
            let cancel = crate::task::opt_token(&opts).map_or_else(CancellationToken::new, |t| t.child_token());
            let guard = CancelOnDrop(cancel.clone());
            let prepared: mlua::Result<_> = (|| {
                let live = parse_live(&lua, &opts)?;
                let args: TraceEventArgs = super::parse_args(&lua, mlua::Value::Table(args))?;
                let (session_id, seq, expected_gen) = if let Some(eid) = &args.event_id {
                    let p: Position = decode(eid)?;
                    if let Some(sid) = &args.session_id
                        && sid != &p.session_id
                    {
                        return Err(super::query_err("QUERY_INVALID_FILTER: session_id mismatches event_id"));
                    }
                    if let Some(s) = args.seq
                        && s != p.seq
                    {
                        return Err(super::query_err("QUERY_INVALID_FILTER: seq mismatches event_id"));
                    }
                    (p.session_id, p.seq, Some(p.generation))
                } else {
                    let sid = args
                        .session_id
                        .filter(|s| !s.is_empty() && s.len() <= 256)
                        .ok_or_else(|| super::query_err("QUERY_INVALID_FILTER: session_id and seq are required"))?;
                    let seq = args.seq.ok_or_else(|| super::query_err("QUERY_INVALID_FILTER: seq is required"))?;
                    (sid, seq, None)
                };
                if seq == 0 {
                    return Err(super::query_err("QUERY_INVALID_FILTER: seq must be >= 1"));
                }
                let source = scope(&lua)?;
                let projector = crate::projection::JsonProjector::from_lua(&lua)?;
                Ok((session_id, seq, expected_gen, source, live, projector))
            })();
            async move {
                let _guard = guard;
                let (session_id, seq, expected_gen, source, live, projector) = prepared?;
                let result = run_blocking(move || {
                    let mut c = corpus_with_live(&source, &cancel, Some(&session_id), None, &live)?;
                    trace_event_in_corpus_projected(&mut c, &session_id, seq, expected_gen.as_deref(), &projector)
                })
                .await?;
                json_to_lua(&lua, &result)
            }
        })?,
    )?;
    store.set(
        "trace_item",
        lua.create_async_function(|lua, (args, opts): (Table, Option<Table>)| {
            let cancel = crate::task::opt_token(&opts).map_or_else(CancellationToken::new, |t| t.child_token());
            let guard = CancelOnDrop(cancel.clone());
            let prepared: mlua::Result<_> = (|| {
                let live = parse_live(&lua, &opts)?;
                let args: ReadItem = super::parse_args(&lua, mlua::Value::Table(args))?;
                if args.before.is_some() || args.after.is_some() || args.offset.is_some() || args.max_bytes.is_some() {
                    return Err(super::coded_err(
                        "invalid_arguments",
                        "trace_item accepts only session_id and item_id",
                    ));
                }
                let source = scope(&lua)?;
                let projector = crate::projection::JsonProjector::from_lua(&lua)?;
                Ok((args, source, live, projector))
            })();
            async move {
                let _guard = guard;
                let (args, source, live, projector) = prepared?;
                let result = run_blocking(move || {
                    let mut c = corpus_with_live(&source, &cancel, Some(&args.session_id), None, &live)?;
                    trace_item_in_corpus_projected(&mut c, &args.session_id, &args.item_id, &projector)
                })
                .await?;
                json_to_lua(&lua, &result)
            }
        })?,
    )
}
