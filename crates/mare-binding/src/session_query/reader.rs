//! Event-level reading with neighborhood summaries and UTF-8 bounded chunking.
use mare_shared::lua::{HISTORY_LOSSLESS_MAX_NODES, json_to_lua, json_to_lua_lossless_bounded, rt_err};
use mlua::{Lua, Table};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::corpus::corpus_with_live;
use super::{
    CancelOnDrop, Corpus, INLINE_EVENT_LIMIT, Position, Read, ReadItem, clip, decode, digest, encode, parse_live,
    scope, text,
};

async fn run_blocking<F>(f: F) -> mlua::Result<Value>
where
    F: FnOnce() -> mlua::Result<Value> + Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(rt_err)?
}

fn chunk_bounds(encoded: &str, offset: Option<usize>, max_bytes: Option<usize>) -> mlua::Result<(usize, usize)> {
    let off = offset.unwrap_or(0);
    if off >= encoded.len() && !(off == 0 && encoded.is_empty()) {
        return Err(super::query_err("QUERY_INVALID_CURSOR: chunk offset exceeds content size"));
    }
    if !encoded.is_char_boundary(off) {
        return Err(super::query_err("QUERY_INVALID_CURSOR: chunk offset must align on UTF-8 boundary"));
    }
    let max_bytes = max_bytes.unwrap_or(INLINE_EVENT_LIMIT).clamp(16, INLINE_EVENT_LIMIT);
    let mut end = off.saturating_add(max_bytes).min(encoded.len());
    while end > off && !encoded.is_char_boundary(end) {
        end -= 1;
    }
    Ok((off, end))
}

/// Identify the inherited seed by its host-owned fork-origin boundary. The messages remain
/// readable in the child transcript, but belong to the source when constructing search documents.
pub(super) fn inherited_prefix(entries: &[Value]) -> mlua::Result<Option<(usize, usize)>> {
    let mut origin = None;
    for (index, entry) in entries.iter().enumerate() {
        if entry["type"] == "custom" && entry["kind"] == "session/fork-origin" {
            if entry["provenance"] != "session-host" {
                return Err(super::coded_err("invalid_arguments", "untrusted session fork-origin record"));
            }
            if origin.is_some() {
                return Err(super::coded_err("invalid_arguments", "duplicate session fork-origin record"));
            }
            let data = entry
                .get("data")
                .cloned()
                .ok_or_else(|| super::coded_err("invalid_arguments", "missing fork-origin data"))?;
            let parsed: mare_store::record::ForkOriginData = serde_json::from_value(data)
                .map_err(|e| super::coded_err("invalid_arguments", format!("invalid fork-origin data: {e}")))?;
            if parsed.source_session_id.is_empty() || parsed.source_leaf_entry_id.is_empty() {
                return Err(super::coded_err("invalid_arguments", "empty fork-origin source identity"));
            }
            origin = Some((index, parsed.inherited_through_entry_id));
        }
    }
    let Some((start, Some(boundary))) = origin else { return Ok(None) };
    let mut matches =
        entries.iter().enumerate().filter(|(_, entry)| entry["entry_id"].as_str() == Some(boundary.as_str()));
    let Some((end, entry)) = matches.next() else {
        return Err(super::coded_err("invalid_arguments", "fork-origin boundary is absent"));
    };
    if boundary.is_empty() || end <= start || entry["type"] != "message" || matches.next().is_some() {
        return Err(super::coded_err("invalid_arguments", "invalid fork-origin boundary"));
    }
    Ok(Some((start, end)))
}

#[cfg(test)]
pub(super) fn read(mut c: Corpus, args: Read) -> mlua::Result<Value> {
    read_projected(&mut c, args, &crate::projection::JsonProjector::default())
}

pub(super) fn read_projected(
    c: &mut Corpus,
    args: Read,
    projector: &crate::projection::JsonProjector,
) -> mlua::Result<Value> {
    let p: Position = decode(&args.event_id)?;
    let entries = c
        .sessions
        .iter_mut()
        .find(|e| e[0]["session_id"] == p.session_id)
        .ok_or_else(|| super::coded_err("unavailable", "event unavailable in this workspace"))?;
    if p.generation != digest(entries) {
        return Err(super::coded_err("stale_session", "stale history event; repeat search"));
    }
    if p.seq == 0 || p.seq >= entries.len() {
        return Err(super::coded_err("unavailable", "event unavailable in this workspace"));
    }
    for entry in entries.iter_mut() {
        projector.project_json(entry);
    }
    let e = &entries[p.seq];
    let (before, after) = (args.before.unwrap_or(0), args.after.unwrap_or(0));
    if before > 10 || after > 10 {
        return Err(super::coded_err("invalid_arguments", "event neighborhood must be 0..10"));
    }
    let encoded = serde_json::to_string(e).map_err(rt_err)?;
    let start = p.seq.saturating_sub(before).max(1);
    let end = (p.seq + after + 1).min(entries.len());
    let neighbors: Vec<_> = (start..end)
        .filter(|seq| *seq != p.seq)
        .map(|seq| {
            let n = Position { generation: p.generation.clone(), session_id: p.session_id.clone(), seq };
            json!({
                "event_id": encode(&n),
                "seq": seq,
                "type": clip(entries[seq]["type"].as_str().unwrap_or("unknown"), 64),
                "summary": clip(&text(&entries[seq]), 512),
            })
        })
        .collect();

    if args.offset.is_some() || args.max_bytes.is_some() {
        let total = encoded.len();
        let (off, slice_end) = chunk_bounds(&encoded, args.offset, args.max_bytes)?;
        let chunk = &encoded[off..slice_end];
        let next_offset = (slice_end < total).then_some(slice_end);
        return Ok(json!({
            "session_id": p.session_id,
            "corpus_revision": p.generation,
            "seq": p.seq,
            "chunk": chunk,
            "chunk_offset": off,
            "next_offset": next_offset,
            "total_bytes": total,
            "truncated": false,
            "neighbors": if off == 0 { neighbors } else { Vec::new() },
        }));
    }

    if encoded.len() > INLINE_EVENT_LIMIT {
        return Err(super::query_err("QUERY_EVENT_TOO_LARGE: event exceeds 64 KiB inline limit"));
    }
    Ok(json!({
        "session_id": p.session_id,
        "corpus_revision": p.generation,
        "seq": p.seq,
        "event": e,
        "neighbors": neighbors,
    }))
}

pub(super) fn read_item_projected(
    c: &mut Corpus,
    args: ReadItem,
    projector: &crate::projection::JsonProjector,
) -> mlua::Result<Value> {
    if args.session_id.is_empty() || args.item_id.is_empty() {
        return Err(super::coded_err("invalid_arguments", "session_id and item_id are required"));
    }
    let entries = c
        .sessions
        .iter_mut()
        .find(|e| e[0]["session_id"] == args.session_id)
        .ok_or_else(|| super::coded_err("unavailable", "session unavailable in this workspace"))?;
    let generation = digest(entries);
    for entry in entries.iter_mut() {
        projector.project_json(entry);
    }
    let items = crate::session_surface::semantic_items(entries);
    let pos = items
        .iter()
        .position(|item| item.value["item_id"] == args.item_id)
        .ok_or_else(|| super::coded_err("session.unknown_item", "item unavailable in this session"))?;
    let (before, after) = (args.before.unwrap_or(0), args.after.unwrap_or(0));
    if before > 10 || after > 10 {
        return Err(super::coded_err("invalid_arguments", "item neighborhood must be 0..10"));
    }
    let start = pos.saturating_sub(before);
    let end = pos.saturating_add(after).saturating_add(1).min(items.len());
    let neighbors: Vec<_> = (start..end)
        .filter(|index| *index != pos)
        .map(|index| {
            let item = &items[index].value;
            json!({ "item_id": item["item_id"], "type": item["type"] })
        })
        .collect();
    let item = &items[pos];
    let encoded = serde_json::to_string(&item.value).map_err(rt_err)?;
    if args.offset.is_some() || args.max_bytes.is_some() {
        let (off, end) = chunk_bounds(&encoded, args.offset, args.max_bytes)?;
        let mut result = json!({
            "session_id": args.session_id, "item_id": args.item_id, "corpus_revision": generation, "seq": item.seq,
            "chunk": &encoded[off..end], "chunk_offset": off, "total_bytes": encoded.len(),
            "neighbors": if off == 0 { neighbors } else { Vec::new() },
        });
        if end < encoded.len() {
            result["next_offset"] = json!(end);
        }
        return Ok(result);
    }
    if encoded.len() > INLINE_EVENT_LIMIT {
        return Err(super::query_err("QUERY_EVENT_TOO_LARGE: item exceeds 64 KiB inline limit"));
    }
    Ok(json!({
        "session_id": args.session_id, "item_id": args.item_id, "corpus_revision": generation, "seq": item.seq,
        "item": item.value, "neighbors": neighbors,
    }))
}

pub(super) fn install(lua: &Lua, store: &Table) -> mlua::Result<()> {
    store.set(
        "read_event",
        lua.create_async_function(|lua, (args, opts): (Table, Option<Table>)| {
            let cancel = crate::task::opt_token(&opts).map_or_else(CancellationToken::new, |t| t.child_token());
            let guard = CancelOnDrop(cancel.clone());
            let prepared: mlua::Result<_> = (|| {
                let live = parse_live(&lua, &opts)?;
                let args: Read = super::parse_args(&lua, mlua::Value::Table(args))?;
                let source = scope(&lua)?;
                let projector = crate::projection::JsonProjector::from_lua(&lua)?;
                Ok((args, source, live, projector))
            })();
            async move {
                let _guard = guard;
                let (args, source, live, projector) = prepared?;
                let result = run_blocking(move || {
                    let position: Position = decode(&args.event_id)?;
                    let mut corpus = corpus_with_live(&source, &cancel, Some(&position.session_id), None, &live)?;
                    read_projected(&mut corpus, args, &projector)
                })
                .await?;
                json_to_lua(&lua, &result)
            }
        })?,
    )?;
    store.set(
        "read_item",
        lua.create_async_function(|lua, (args, opts): (Table, Option<Table>)| {
            let cancel = crate::task::opt_token(&opts).map_or_else(CancellationToken::new, |t| t.child_token());
            let guard = CancelOnDrop(cancel.clone());
            let prepared: mlua::Result<_> = (|| {
                let live = parse_live(&lua, &opts)?;
                let args: ReadItem = super::parse_args(&lua, mlua::Value::Table(args))?;
                let source = scope(&lua)?;
                let projector = crate::projection::JsonProjector::from_lua(&lua)?;
                Ok((args, source, live, projector))
            })();
            async move {
                let _guard = guard;
                let (args, source, live, projector) = prepared?;
                let result = run_blocking(move || {
                    let mut corpus = corpus_with_live(&source, &cancel, Some(&args.session_id), None, &live)?;
                    read_item_projected(&mut corpus, args, &projector)
                })
                .await?;
                json_to_lua_lossless_bounded(&lua, &result, HISTORY_LOSSLESS_MAX_NODES)
            }
        })?,
    )
}
