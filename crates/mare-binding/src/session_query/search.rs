//! Batched session and event search over authorized persisted and live sources.
use mare_shared::lua::{HISTORY_LOSSLESS_MAX_NODES, json_to_lua_lossless_bounded, rt_err};
use mare_store::search::SortOrder;
use mlua::{Lua, Table};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use tokio_util::sync::CancellationToken;

use super::corpus::{
    Catalog, EligibleSource, ReadErr, SourceKind, SourceObservation, availability_flags, catalog,
    ensure_stamps_unchanged, overlay_headers, read_source_entries, record_logical_generation, validate_live,
    validate_source_identity,
};
use super::{
    CancelOnDrop, Cursor, LiveSessionInput, PAGE, Position, Search, SourceScope, TOTAL_BYTES, decode, digest, encode,
    parse_live, scope, session_timestamps, text, title,
};
use crate::session_filters::SessionFilters as Filters;

struct HitMeta {
    session_id: String,
    session_digest: String,
    title: String,
    archived: bool,
    is_live: bool,
    created_at: i64,
    updated_at: i64,
    surface: &'static str,
    event_type: String,
    timestamp: Value,
    seq: usize,
    item_id: Option<String>,
    item: Option<Value>,
}

fn build_seq_item_ids(entries: &[Value]) -> (HashMap<usize, String>, HashSet<String>) {
    let mut by_seq = HashMap::new();
    let mut all_ids = HashSet::new();
    for item in crate::session_surface::semantic_items(entries) {
        if let Some(id) = item.value["item_id"].as_str().filter(|s| !s.is_empty()) {
            all_ids.insert(id.to_owned());
            by_seq.entry(item.seq).or_insert_with(|| id.to_owned());
        }
    }
    (by_seq, all_ids)
}

fn item_id_for_seq(
    entries: &[Value],
    seq: usize,
    by_seq: &HashMap<usize, String>,
    all_ids: &HashSet<String>,
) -> Option<String> {
    if let Some(id) = by_seq.get(&seq) {
        return Some(id.clone());
    }
    let entry = entries.get(seq)?;
    if entry["type"] == "message"
        && entry["message"]["role"] == "tool_result"
        && let Some(call_id) = entry["message"]["tool_call_id"].as_str()
        && all_ids.contains(call_id)
    {
        return Some(call_id.to_owned());
    }
    None
}

pub(super) type EligibleBundle = (Vec<EligibleSource>, Vec<Value>, Vec<SourceObservation>, bool);

pub(super) fn validate_text_query(q: &str, kind: &str) -> mlua::Result<String> {
    if q.contains('\0') || q.chars().count() > 512 || q.trim().is_empty() {
        return Err(super::query_err(format!("QUERY_INVALID_QUERY: {kind} must contain 1..512 non-NUL characters")));
    }
    Ok(q.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub(super) fn pattern(q: &str) -> mlua::Result<regex::Regex> {
    let norm = validate_text_query(q, "query")?;
    let expression = norm.split_whitespace().map(regex::escape).collect::<Vec<_>>().join(r"\s+");
    regex::RegexBuilder::new(&expression)
        .case_insensitive(true)
        .build()
        .map_err(|e| super::coded_err("invalid_arguments", e.to_string()))
}

#[cfg(test)]
pub(super) fn snippet(e: &Value, pattern: &regex::Regex) -> Option<(String, usize)> {
    let t = text(e);
    let hit = pattern.find(&t)?;
    let start = t[..hit.start()].chars().count();
    let matched = t[hit.start()..hit.end()].chars().count();
    Some((t.chars().skip(start.saturating_sub(40)).take(matched + 80).collect(), start))
}

fn parse_sort_order(
    sort_key: Option<&str>,
    sort_direction: Option<&str>,
    has_query: bool,
) -> mlua::Result<(SortOrder, &'static str, &'static str)> {
    let key = match sort_key {
        None => {
            if has_query {
                "relevance"
            } else {
                "updated_at_ms"
            }
        }
        Some("relevance") => "relevance",
        Some("created_at_ms") => "created_at_ms",
        Some("updated_at_ms") => "updated_at_ms",
        Some(_) => return Err(super::query_err("QUERY_INVALID_FILTER: unsupported sort_key")),
    };
    let dir = match sort_direction {
        None | Some("desc") => "desc",
        Some("asc") => "asc",
        Some(_) => return Err(super::query_err("QUERY_INVALID_FILTER: unsupported sort_direction")),
    };
    let order = match (key, dir) {
        ("relevance", _) => SortOrder::Relevance,
        ("created_at_ms", "desc") => SortOrder::CreatedDesc,
        ("created_at_ms", "asc") => SortOrder::CreatedAsc,
        ("updated_at_ms", "desc") => SortOrder::UpdatedDesc,
        ("updated_at_ms", "asc") => SortOrder::UpdatedAsc,
        _ => unreachable!(),
    };
    Ok((order, key, dir))
}

pub(super) fn build_eligible_sources(
    scope: &SourceScope,
    cancel: &CancellationToken,
    args: &Search,
    events: bool,
    live_inputs: &[LiveSessionInput],
    predicate: &Filters,
) -> mlua::Result<EligibleBundle> {
    let Catalog { headers: disk_headers, sources, incomplete } = catalog(scope, cancel)?;
    let (wants_live, wants_persisted) = availability_flags(predicate);
    let read_scope = scope.policy.as_ref().map(|policy| (policy.read_roots(), format!("{:?}", policy.deny_read)));
    let scope_key = digest(&(scope.cwd.as_str(), scope.owner.as_str(), read_scope));
    let apply_step_boundary = args.step_bound != Some(false);
    let live_records = if wants_live { validate_live(scope, live_inputs, apply_step_boundary) } else { Vec::new() };
    let headers = overlay_headers(&disk_headers, &live_records);

    let mut eligible = Vec::new();
    let mut seen_live = HashSet::new();
    for rec in live_records {
        if (!events || args.session_id.as_deref() == Some(rec.id.as_str()))
            && predicate.accepts_with_availability(std::slice::from_ref(&rec.header), &headers, true)?
        {
            seen_live.insert(rec.id.clone());
            let stamp_key = digest(&(scope_key.as_str(), &rec.session_epoch, &rec.entries));
            eligible.push(EligibleSource {
                id: rec.id,
                header: rec.header,
                is_live: true,
                kind: SourceKind::Live(rec.entries),
                stamp_key,
            });
        }
    }
    if wants_persisted {
        for (path, stamp, h) in &sources {
            let id = h["session_id"].as_str().unwrap_or_default().to_owned();
            if seen_live.contains(&id) {
                continue;
            }
            if (!events || args.session_id.as_deref() == Some(id.as_str()))
                && predicate.accepts_with_availability(std::slice::from_ref(h), &headers, false)?
            {
                let stamp_key = digest(&(scope_key.as_str(), path.to_string_lossy(), &id, stamp.len, stamp.mtime_ns));
                eligible.push(EligibleSource {
                    id,
                    header: h.clone(),
                    is_live: false,
                    kind: SourceKind::Disk(path.clone(), *stamp),
                    stamp_key,
                });
            }
        }
    }
    Ok((eligible, headers, sources, incomplete))
}

struct ValidatedSearch {
    q: String,
    has_query: bool,
    title_filter: Option<String>,
    filters: Filters,
    event_filters: crate::session_filters::EventFilters,
    order: SortOrder,
    sort_key_name: &'static str,
    sort_dir_name: &'static str,
}

fn validate_search_args(args: &Search) -> mlua::Result<ValidatedSearch> {
    if args.limit == Some(0) {
        return Err(super::coded_err("invalid_arguments", "limit must be a positive integer"));
    }
    let has_query = !args.query.trim().is_empty();
    let has_title = args.title_query.as_deref().is_some_and(|t| !t.trim().is_empty());
    if !has_query && !has_title {
        return Err(super::query_err("QUERY_INVALID_QUERY: query or title_query must be non-empty"));
    }
    let q = if has_query {
        let _ = pattern(&args.query)?;
        validate_text_query(&args.query, "query")?.to_lowercase()
    } else {
        if args.query.contains('\0') || args.query.chars().count() > 512 {
            return Err(super::query_err("QUERY_INVALID_QUERY: query must contain 1..512 non-NUL characters"));
        }
        String::new()
    };
    let title_filter = args
        .title_query
        .as_deref()
        .map(|t| validate_text_query(t, "title_query"))
        .transpose()?
        .map(|s| s.to_lowercase());

    let filters = args.filters.clone().unwrap_or_default();
    filters.validate()?;
    let event_filters = args.event_filters.clone().unwrap_or_default();
    event_filters.validate()?;
    let (order, sort_key_name, sort_dir_name) =
        parse_sort_order(args.sort_key.as_deref(), args.sort_direction.as_deref(), has_query)?;

    Ok(ValidatedSearch { q, has_query, title_filter, filters, event_filters, order, sort_key_name, sort_dir_name })
}

fn sort_eligible_by_created(eligible: &mut [EligibleSource], events: bool, order: SortOrder) {
    if !events && matches!(order, SortOrder::CreatedAsc | SortOrder::CreatedDesc) {
        eligible.sort_by(|a, b| {
            let ca = a.header["created_at_ms"].as_i64().unwrap_or(0);
            let cb = b.header["created_at_ms"].as_i64().unwrap_or(0);
            let cmp = if order == SortOrder::CreatedDesc { cb.cmp(&ca) } else { ca.cmp(&cb) };
            cmp.then_with(|| a.id.cmp(&b.id))
        });
    }
}

fn resolve_cursor(
    token: Option<String>,
    generation: &str,
    identity: &str,
    eligible_len: usize,
) -> mlua::Result<(usize, usize)> {
    let Some(token) = token else {
        return Ok((0, 0));
    };
    let cursor: Cursor = decode(&token)?;
    if cursor.generation != generation || cursor.identity != identity {
        return Err(mlua::Error::external(mare_shared::errors::Coded {
            code: "stale_session",
            message: "stale history cursor; restart search".into(),
        }));
    }
    if cursor.batch_start > 0 && cursor.batch_start >= eligible_len {
        return Err(super::coded_err("invalid_arguments", "invalid history cursor"));
    }
    Ok((cursor.batch_start, cursor.offset))
}

fn search_page(data: Vec<Value>, next_cursor: Option<String>, corpus_revision: String, incomplete: bool) -> Value {
    let mut result = json!({"data": data, "corpus_revision": corpus_revision, "incomplete": incomplete});
    if let Some(cursor) = next_cursor {
        result["next_cursor"] = json!(cursor);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn search_title_only(
    eligible: &[EligibleSource],
    headers: &[Value],
    disk_sources: &[SourceObservation],
    mut incomplete: bool,
    validated: &ValidatedSearch,
    generation: String,
    identity: String,
    offset: usize,
    limit: usize,
    cancel: &CancellationToken,
    projector: &crate::projection::JsonProjector,
) -> mlua::Result<Value> {
    let mut sessions_out = Vec::new();
    for src in eligible {
        if cancel.is_cancelled() {
            return Err(super::query_err("QUERY_CANCELLED: query cancelled"));
        }
        let mut entries = match read_source_entries(&src.kind, cancel) {
            Ok((v, _)) => v,
            Err(ReadErr::Budget) => {
                return Err(super::query_err(
                    "QUERY_BUDGET_EXCEEDED: session file exceeds 8 MiB limit; narrow session_ids",
                ));
            }
            Err(ReadErr::Cancelled) => return Err(super::query_err("QUERY_CANCELLED: query cancelled")),
            Err(ReadErr::Incomplete) => {
                incomplete = true;
                continue;
            }
        };
        let session_digest = digest(&entries);
        for entry in &mut entries {
            projector.project_json(entry);
        }
        if !validated.filters.accepts_with_availability(&entries, headers, src.is_live)? {
            continue;
        }
        let session_title = title(&entries);
        if let Some(ref tf) = validated.title_filter
            && !session_title.to_lowercase().contains(tf)
        {
            continue;
        }
        let (created_at, updated_at) = session_timestamps(&entries);
        let proj = crate::session_surface::project(&entries)?;
        let (by_seq, all_item_ids) = build_seq_item_ids(&entries);
        let chosen_seq = (1..entries.len())
            .rev()
            .find(|&seq| {
                validated
                    .event_filters
                    .accepts_with_surface(&entries[seq], seq, Some(proj.surfaces[seq].as_str()))
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| entries.len().saturating_sub(1));
        let p = Position { generation: session_digest, session_id: src.id.clone(), seq: chosen_seq };
        let chosen_surface = proj.surfaces.get(chosen_seq).copied().unwrap_or(crate::session_surface::Surface::LogOnly);
        let chosen_entry = entries.get(chosen_seq);
        let mut hit = json!({
            "session_id": src.id,
            "title": session_title,
            "event_id": encode(&p),
            "position": { "seq": chosen_seq, "text_offset": 0 },
            "snippet": session_title,
            "surface": chosen_surface.as_str(),
            "type": chosen_entry.and_then(|e| e["type"].as_str()).unwrap_or("session"),
            "session_created_at_ms": created_at,
            "updated_at_ms": updated_at,
            "archived": crate::session_filters::archived(&entries),
            "availability": if src.is_live { "live" } else { "persisted" },
            "rank": { "match_count": 1, "document_length": session_title.chars().count() },
        });
        if let Some(item_id) = item_id_for_seq(&entries, chosen_seq, &by_seq, &all_item_ids) {
            hit["item_id"] = json!(item_id);
        }
        if let Some(ts) = chosen_entry.and_then(|e| e["created_at_ms"].as_i64()) {
            hit["matched_at_ms"] = json!(ts);
        }
        sessions_out.push((created_at, updated_at, hit));
    }
    ensure_stamps_unchanged(disk_sources)?;
    sessions_out.sort_by(|a, b| {
        let cmp = match validated.order {
            SortOrder::CreatedDesc => b.0.cmp(&a.0),
            SortOrder::CreatedAsc => a.0.cmp(&b.0),
            SortOrder::UpdatedAsc => a.1.cmp(&b.1),
            SortOrder::UpdatedDesc | SortOrder::Relevance => b.1.cmp(&a.1),
        };
        cmp.then_with(|| a.2["session_id"].as_str().cmp(&b.2["session_id"].as_str()))
    });
    if offset > sessions_out.len() || (offset > 0 && offset == sessions_out.len()) {
        return Err(super::coded_err("invalid_arguments", "invalid history cursor"));
    }
    let end = offset.saturating_add(limit).min(sessions_out.len());
    let more = end < sessions_out.len();
    let hits: Vec<Value> = sessions_out[offset..end].iter().map(|x| x.2.clone()).collect();
    let cursor =
        more.then(|| encode(&Cursor { generation: generation.clone(), identity, batch_start: 0, offset: end }));
    Ok(search_page(hits, cursor, generation, incomplete))
}

struct CollectedBatch {
    documents: Vec<mare_store::search::Document>,
    metas: Vec<HitMeta>,
    next_batch: usize,
    incomplete: bool,
}

struct BatchRequest {
    start: usize,
    semantic_items: bool,
}

fn collect_search_batch(
    eligible: &[EligibleSource],
    headers: &[Value],
    mut incomplete: bool,
    validated: &ValidatedSearch,
    request: BatchRequest,
    cancel: &CancellationToken,
    projector: &crate::projection::JsonProjector,
) -> mlua::Result<CollectedBatch> {
    let mut batch_bytes = 0u64;
    let mut next_batch = eligible.len();
    let mut documents = Vec::new();
    let mut metas = Vec::new();
    let mut seen_logical: HashMap<String, String> = HashMap::new();

    for (idx, src) in eligible.iter().enumerate().skip(request.start) {
        if cancel.is_cancelled() {
            return Err(super::query_err("QUERY_CANCELLED: query cancelled"));
        }
        let file_len = match &src.kind {
            SourceKind::Disk(_, s) => s.len,
            SourceKind::Live(e) => serde_json::to_vec(e).map(|b| b.len() as u64).unwrap_or(0),
        };
        if file_len > mare_store::bounded::MAX_BYTES {
            return Err(super::query_err(
                "QUERY_BUDGET_EXCEEDED: snapshot exceeds 8 MiB per-session limit; narrow filters",
            ));
        }
        if batch_bytes > 0 && batch_bytes + file_len > TOTAL_BYTES {
            next_batch = idx;
            break;
        }
        let (mut entries, bytes_read) = match read_source_entries(&src.kind, cancel) {
            Ok(v) => v,
            Err(ReadErr::Budget) => {
                return Err(super::query_err(
                    "QUERY_BUDGET_EXCEEDED: snapshot exceeds 8 MiB per-session limit; narrow filters",
                ));
            }
            Err(ReadErr::Cancelled) => return Err(super::query_err("QUERY_CANCELLED: query cancelled")),
            Err(ReadErr::Incomplete) => {
                incomplete = true;
                continue;
            }
        };
        batch_bytes += bytes_read;
        validate_source_identity(&entries, &src.header)?;
        let session_digest = digest(&entries);
        if !record_logical_generation(&mut seen_logical, src.id.clone(), session_digest.clone())? {
            continue;
        }
        let inherited = match super::reader::inherited_prefix(&entries) {
            Ok(inherited) => inherited,
            Err(_) => {
                incomplete = true;
                continue;
            }
        };
        for entry in &mut entries {
            projector.project_json(entry);
        }
        if !validated.filters.accepts_with_availability(&entries, headers, src.is_live)? {
            continue;
        }
        let session_title = title(&entries);
        if let Some(ref tf) = validated.title_filter
            && !session_title.to_lowercase().contains(tf)
        {
            continue;
        }
        let session_archived = crate::session_filters::archived(&entries);
        let (created_at, updated_at) = session_timestamps(&entries);
        let proj = crate::session_surface::project(&entries)?;
        let (by_seq, all_item_ids) =
            if request.semantic_items { (HashMap::new(), HashSet::new()) } else { build_seq_item_ids(&entries) };
        let mut candidates: Vec<(usize, Option<Value>)> = if request.semantic_items {
            crate::session_surface::semantic_items(&entries)
                .into_iter()
                .map(|item| (item.seq, Some(item.value)))
                .collect()
        } else {
            (1..entries.len()).map(|seq| (seq, None)).collect()
        };
        if request.semantic_items {
            candidates.sort_by(|a, b| {
                a.0.cmp(&b.0).then_with(|| {
                    a.1.as_ref()
                        .and_then(|item| item["item_id"].as_str())
                        .cmp(&b.1.as_ref().and_then(|item| item["item_id"].as_str()))
                })
            });
        }
        for (seq, item) in candidates {
            let entry = &entries[seq];
            if cancel.is_cancelled() {
                return Err(super::query_err("QUERY_CANCELLED: query cancelled"));
            }
            if entry["type"] == "message" && inherited.is_some_and(|(start, end)| seq > start && seq <= end) {
                continue;
            }
            let surface = proj.surfaces[seq].as_str();
            let item_type = item.as_ref().and_then(|value| value["type"].as_str());
            if !validated.event_filters.accepts_with_surface_and_type(entry, seq, Some(surface), item_type)? {
                continue;
            }
            let content = if item.as_ref().is_some_and(|value| value["type"] == "tool_call") {
                let item = item.as_ref().unwrap();
                let args = if item["arguments"].is_null() { String::new() } else { item["arguments"].to_string() };
                format!(
                    "{} {} {} {}",
                    item["tool_call_id"].as_str().unwrap_or_default(),
                    item["name"].as_str().unwrap_or_default(),
                    args,
                    text(&json!({ "type": "message", "message": item["message"] }))
                )
            } else {
                text(entry)
            };
            if content.trim().is_empty() && validated.has_query {
                continue;
            }
            documents.push(mare_store::search::Document {
                session_id: src.id.clone(),
                seq,
                text: content,
                timestamp: entry["created_at_ms"].as_i64().unwrap_or(0),
                created_at,
                updated_at,
            });
            metas.push(HitMeta {
                session_id: src.id.clone(),
                session_digest: session_digest.clone(),
                title: session_title.clone(),
                archived: session_archived,
                is_live: src.is_live,
                created_at,
                updated_at,
                surface,
                event_type: item
                    .as_ref()
                    .and_then(|value| value["type"].as_str())
                    .or_else(|| entries[seq]["type"].as_str())
                    .unwrap_or("unknown")
                    .to_owned(),
                timestamp: entries[seq].get("created_at_ms").cloned().unwrap_or(Value::Null),
                seq,
                item_id: if item.is_some() { None } else { item_id_for_seq(&entries, seq, &by_seq, &all_item_ids) },
                item,
            });
        }
    }

    Ok(CollectedBatch { documents, metas, next_batch, incomplete })
}

fn format_ranked_hits(
    ranked_hits: Vec<mare_store::search::Hit>,
    documents: &[mare_store::search::Document],
    metas: &[HitMeta],
) -> Vec<Value> {
    let mut hits = Vec::with_capacity(ranked_hits.len());
    for hit in ranked_hits {
        let meta = &metas[hit.document];
        let p =
            Position { generation: meta.session_digest.clone(), session_id: meta.session_id.clone(), seq: meta.seq };
        let mut value = json!({
            "session_id": p.session_id,
            "title": meta.title,
            "position": { "seq": meta.seq, "text_offset": hit.text_offset },
            "snippet": hit.snippet,
            "surface": meta.surface,
            "type": meta.event_type,
            "session_created_at_ms": meta.created_at,
            "updated_at_ms": meta.updated_at,
            "archived": meta.archived,
            "availability": if meta.is_live { "live" } else { "persisted" },
            "rank": { "match_count": hit.match_count, "document_length": documents[hit.document].text.chars().count() },
        });
        if let Some(ts) = meta.timestamp.as_i64() {
            value["matched_at_ms"] = json!(ts);
        }
        if let Some(item) = &meta.item {
            for (key, field) in item.as_object().expect("semantic item is an object") {
                value[key] = field.clone();
            }
        } else {
            value["event_id"] = json!(encode(&p));
            if let Some(item_id) = &meta.item_id {
                value["item_id"] = json!(item_id);
            }
        }
        hits.push(value);
    }
    hits
}

#[allow(clippy::too_many_arguments)]
pub(super) fn search_sources(
    mut eligible: Vec<EligibleSource>,
    headers: &[Value],
    disk_sources: &[SourceObservation],
    incomplete: bool,
    args: Search,
    events: bool,
    items: bool,
    owner: &str,
    cancel: &CancellationToken,
    projector: &crate::projection::JsonProjector,
) -> mlua::Result<Value> {
    let validated = validate_search_args(&args)?;

    if events && eligible.is_empty() {
        return Err(super::coded_err("unavailable", "session unavailable in this workspace"));
    }

    sort_eligible_by_created(&mut eligible, events, validated.order);

    let stamp_keys: Vec<(&str, bool, &str)> =
        eligible.iter().map(|s| (s.id.as_str(), s.is_live, s.stamp_key.as_str())).collect();
    let generation = digest(&stamp_keys);
    let identity = digest(&(
        events,
        items,
        owner,
        &validated.q,
        &validated.title_filter,
        validated.sort_key_name,
        validated.sort_dir_name,
        &args.session_id,
        &validated.filters,
        &validated.event_filters,
        args.step_bound != Some(false),
    ));

    let limit = args.limit.unwrap_or(PAGE);
    let (batch_start, offset) = resolve_cursor(args.cursor, &generation, &identity, eligible.len())?;

    // Title-only session list mode: filter and sort matching sessions directly.
    if !validated.has_query && !items {
        if batch_start != 0 {
            return Err(super::coded_err("invalid_arguments", "invalid history cursor"));
        }
        return search_title_only(
            &eligible,
            headers,
            disk_sources,
            incomplete,
            &validated,
            generation,
            identity,
            offset,
            limit,
            cancel,
            projector,
        );
    }

    // Stream eligible sessions in <= TOTAL_BYTES batches, dropping raw JSON ASTs after extraction.
    let batch = collect_search_batch(
        &eligible,
        headers,
        incomplete,
        &validated,
        BatchRequest { start: batch_start, semantic_items: items },
        cancel,
        projector,
    )?;
    ensure_stamps_unchanged(disk_sources)?;
    if offset > batch.documents.len() {
        return Err(super::coded_err("invalid_arguments", "invalid history cursor"));
    }

    let token = cancel.clone();
    let ranked = if !validated.has_query {
        let mut positions: Vec<usize> = (0..batch.documents.len()).collect();
        positions.sort_by(|&a, &b| {
            let (a, b) = (&batch.metas[a], &batch.metas[b]);
            let cmp = match validated.order {
                SortOrder::CreatedAsc => a.created_at.cmp(&b.created_at),
                SortOrder::CreatedDesc => b.created_at.cmp(&a.created_at),
                SortOrder::UpdatedAsc => a.updated_at.cmp(&b.updated_at),
                SortOrder::UpdatedDesc | SortOrder::Relevance => b.updated_at.cmp(&a.updated_at),
            };
            cmp.then_with(|| a.session_id.cmp(&b.session_id)).then_with(|| b.seq.cmp(&a.seq)).then_with(|| {
                a.item
                    .as_ref()
                    .and_then(|item| item["item_id"].as_str())
                    .cmp(&b.item.as_ref().and_then(|item| item["item_id"].as_str()))
            })
        });
        let end = offset.saturating_add(limit).min(positions.len());
        let hits = positions[offset..end]
            .iter()
            .map(|&document| mare_store::search::Hit {
                document,
                snippet: super::clip(&batch.documents[document].text, 160),
                text_offset: 0,
                match_count: 0,
            })
            .collect();
        mare_store::search::Page { hits, more: end < positions.len() }
    } else {
        mare_store::search::page_sorted(
            &batch.documents,
            &validated.q,
            !events,
            validated.order,
            offset,
            limit.min(batch.documents.len().max(1)),
            std::sync::Arc::new(move || token.is_cancelled()),
        )
        .map_err(|_| {
            super::query_err(if cancel.is_cancelled() {
                "QUERY_CANCELLED: query cancelled"
            } else {
                "QUERY_INDEX_FAILED: session search index failed"
            })
        })?
    };

    if ranked.hits.is_empty() && offset > 0 {
        return Err(super::coded_err("invalid_arguments", "invalid history cursor"));
    }

    let count = ranked.hits.len();
    let hits = format_ranked_hits(ranked.hits, &batch.documents, &batch.metas);
    let cursor = if ranked.more {
        Some(encode(&Cursor { generation: generation.clone(), identity, batch_start, offset: offset + count }))
    } else if batch.next_batch < eligible.len() {
        Some(encode(&Cursor { generation: generation.clone(), identity, batch_start: batch.next_batch, offset: 0 }))
    } else {
        None
    };
    Ok(search_page(hits, cursor, generation, batch.incomplete))
}

#[cfg(test)]
pub(super) fn search(
    c: super::Corpus,
    args: Search,
    events: bool,
    items: bool,
    cancel: &CancellationToken,
) -> mlua::Result<Value> {
    let eligible: Vec<EligibleSource> = c
        .sessions
        .into_iter()
        .map(|entries| {
            let id = entries[0]["session_id"].as_str().unwrap_or_default().to_owned();
            let is_live = c.live_ids.contains(&id);
            let stamp_key = digest(&entries);
            let header = entries[0].clone();
            EligibleSource { id, header, is_live, kind: SourceKind::Live(entries), stamp_key }
        })
        .collect();
    let headers: Vec<Value> = eligible.iter().map(|s| s.header.clone()).collect();
    let predicate = args.filters.clone().unwrap_or_default();
    predicate.validate()?;
    let filtered: Vec<EligibleSource> = eligible
        .into_iter()
        .filter(|s| {
            (!events || args.session_id.as_deref() == Some(s.id.as_str()))
                && predicate
                    .accepts_with_availability(std::slice::from_ref(&s.header), &headers, s.is_live)
                    .unwrap_or(false)
        })
        .collect();
    search_sources(
        filtered,
        &headers,
        &[],
        c.incomplete,
        args,
        events,
        items,
        "host",
        cancel,
        &crate::projection::JsonProjector::default(),
    )
}

pub(super) fn install(lua: &Lua, store: &Table) -> mlua::Result<()> {
    for (name, events, items) in
        [("search_sessions", false, false), ("search_events", true, false), ("search_items", true, true)]
    {
        store.set(
            name,
            lua.create_async_function(move |lua, (args, opts): (Table, Option<Table>)| {
                let cancel = crate::task::opt_token(&opts).map_or_else(CancellationToken::new, |t| t.child_token());
                let guard = CancelOnDrop(cancel.clone());
                let prepared: mlua::Result<_> = (|| {
                    let live = parse_live(&lua, &opts)?;
                    let args: Search = super::parse_args(&lua, mlua::Value::Table(args))?;
                    if events && args.session_id.is_none() {
                        return Err(super::coded_err("invalid_arguments", "session_id is required"));
                    }
                    if !events && args.session_id.is_some() {
                        return Err(super::coded_err("invalid_arguments", "use filters for session search"));
                    }
                    let source = scope(&lua)?;
                    if !args.query.trim().is_empty() {
                        pattern(&args.query)?;
                    } else if args.title_query.as_deref().is_none_or(|t| t.trim().is_empty()) {
                        return Err(super::query_err(
                            "QUERY_INVALID_QUERY: query must contain 1..512 non-NUL characters",
                        ));
                    }
                    if let Some(tq) = &args.title_query {
                        validate_text_query(tq, "title_query")?;
                    }
                    parse_sort_order(
                        args.sort_key.as_deref(),
                        args.sort_direction.as_deref(),
                        !args.query.trim().is_empty(),
                    )?;
                    if let Some(filters) = &args.filters {
                        filters.validate()?;
                    }
                    if let Some(filters) = &args.event_filters {
                        filters.validate()?;
                    }
                    let projector = crate::projection::JsonProjector::from_lua(&lua)?;
                    Ok((args, source, live, projector))
                })();
                async move {
                    let _guard = guard;
                    let (args, source, live, projector) = prepared?;
                    let result = tokio::task::spawn_blocking(move || {
                        let mut predicate = args.filters.clone().unwrap_or_default();
                        predicate.archived = None;
                        predicate.validate()?;
                        let (eligible, headers, disk_sources, incomplete) =
                            build_eligible_sources(&source, &cancel, &args, events, &live, &predicate)?;
                        let mut result = search_sources(
                            eligible,
                            &headers,
                            &disk_sources,
                            incomplete,
                            args,
                            events,
                            items,
                            &source.owner,
                            &cancel,
                            &projector,
                        )?;
                        projector.project_json(&mut result);
                        Ok::<_, mlua::Error>(result)
                    })
                    .await
                    .map_err(rt_err)??;
                    json_to_lua_lossless_bounded(&lua, &result, HISTORY_LOSSLESS_MAX_NODES)
                }
            })?,
        )?;
    }
    Ok(())
}
