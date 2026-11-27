//! Session corpus discovery, live overlay, and bounded snapshot reading.
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

use super::{CATALOG_LIMIT, Corpus, LiveSessionInput, SourceScope, TOTAL_BYTES, digest};
use crate::session_filters::SessionFilters as Filters;

pub(super) type SourceObservation = (PathBuf, mare_store::FileStamp, Value);

pub(super) struct Catalog {
    pub headers: Vec<Value>,
    pub sources: Vec<SourceObservation>,
    pub incomplete: bool,
}

pub(super) struct LiveRecord {
    pub id: String,
    pub header: Value,
    pub entries: Vec<Value>,
    pub session_epoch: String,
}

#[derive(Clone)]
pub(super) enum SourceKind {
    Disk(PathBuf, mare_store::FileStamp),
    Live(Vec<Value>),
}

#[derive(Clone)]
pub(super) struct EligibleSource {
    pub id: String,
    pub header: Value,
    pub is_live: bool,
    pub kind: SourceKind,
    pub stamp_key: String,
}

pub(super) enum ReadErr {
    Budget,
    Cancelled,
    Incomplete,
}

fn is_session_file(path: &Path) -> bool {
    path.extension().is_some_and(|x| x == "jsonl") || mare_store::bounded::is_gzip(path)
}

fn sanitize_header(h: &Value) -> mlua::Result<Option<Value>> {
    if !h["session_id"].as_str().is_some_and(|id| !id.is_empty() && id.len() <= 256) {
        return Ok(None);
    }
    if !h["parent_session_id"].is_null()
        && !h["parent_session_id"].as_str().is_some_and(|id| !id.is_empty() && id.len() <= 256)
    {
        return Err(super::coded_err("corrupt", "invalid parent_session_id header"));
    }
    Ok(Some(json!({
        "session_id": h["session_id"],
        "cwd": h["cwd"],
        "created_at_ms": h["created_at_ms"].as_i64(),
        "parent_session_id": h["parent_session_id"],
    })))
}

pub(super) fn availability_flags(predicate: &Filters) -> (bool, bool) {
    let wants_live = predicate.availability.as_ref().is_none_or(|a| a.iter().any(|v| v == "live"));
    let wants_persisted = predicate.availability.as_ref().is_none_or(|a| a.iter().any(|v| v == "persisted"));
    (wants_live, wants_persisted)
}

pub(super) fn validate_source_identity(entries: &[Value], header: &Value) -> mlua::Result<()> {
    if entries.first().is_none_or(|e| e["session_id"] != header["session_id"] || e["cwd"] != header["cwd"]) {
        return Err(super::query_err("QUERY_SOURCE_CHANGED: source identity changed; retry"));
    }
    Ok(())
}

/// Returns `Ok(true)` if this is the first observation for `id`, `Ok(false)` if an identical
/// generation was already recorded, or `QUERY_SOURCE_CONFLICT` on divergence.
pub(super) fn record_logical_generation(
    seen: &mut HashMap<String, String>,
    id: String,
    generation: String,
) -> mlua::Result<bool> {
    if let Some(previous) = seen.insert(id, generation.clone()) {
        if previous != generation {
            return Err(super::query_err("QUERY_SOURCE_CONFLICT: inconsistent observations for one logical session"));
        }
        return Ok(false);
    }
    Ok(true)
}

pub(super) fn ensure_stamps_unchanged(sources: &[SourceObservation]) -> mlua::Result<()> {
    if sources.iter().any(|(path, stamp, _)| mare_store::stamp(path).ok() != Some(*stamp)) {
        return Err(super::query_err("QUERY_SOURCE_CHANGED: history changed during observation; retry"));
    }
    Ok(())
}

pub(super) fn catalog(scope: &SourceScope, cancel: &CancellationToken) -> mlua::Result<Catalog> {
    if scope.policy.as_ref().is_some_and(|p| !p.can_read(&scope.dir)) {
        return Err(super::query_err("QUERY_UNAUTHORIZED: query source is no longer readable"));
    }
    let mut directories = vec![(scope.dir.clone(), false, 0usize)];
    let (mut paths, mut directory_count) = (Vec::new(), 0usize);
    while let Some((directory, runs, depth)) = directories.pop() {
        if cancel.is_cancelled() {
            return Err(super::query_err("QUERY_CANCELLED: query cancelled"));
        }
        if scope.policy.as_ref().is_some_and(|p| !p.can_read(&directory)) {
            continue;
        }
        directory_count += 1;
        if directory_count > CATALOG_LIMIT || depth > 32 {
            return Err(super::query_err("QUERY_BUDGET_EXCEEDED: source directory limit exceeded"));
        }
        let listing = match std::fs::read_dir(&directory) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(super::query_err("QUERY_STORAGE_UNAVAILABLE: query source unavailable")),
        };
        for item in listing {
            if cancel.is_cancelled() {
                return Err(super::query_err("QUERY_CANCELLED: query cancelled"));
            }
            let item = item.map_err(|_| super::query_err("QUERY_STORAGE_UNAVAILABLE: query source unavailable"))?;
            let path = item.path();
            if scope.policy.as_ref().is_some_and(|p| !p.can_read(&path)) {
                continue;
            }
            let kind = item
                .file_type()
                .map_err(|_| super::query_err("QUERY_STORAGE_UNAVAILABLE: query source unavailable"))?;
            if kind.is_dir() {
                if runs {
                    directories.push((path, false, depth + 1));
                } else if item.file_name() == "agents" {
                    directories.push((path, true, depth + 1));
                }
                continue;
            }
            if runs || !kind.is_file() || !is_session_file(&path) {
                continue;
            }
            if paths.len() == CATALOG_LIMIT {
                return Err(super::query_err("QUERY_BUDGET_EXCEEDED: source catalogue exceeds its limit"));
            }
            paths.push(path);
        }
    }
    paths.sort();
    let (mut headers, mut sources, mut incomplete) = (Vec::new(), Vec::new(), false);
    for path in paths {
        if cancel.is_cancelled() {
            return Err(super::query_err("QUERY_CANCELLED: query cancelled"));
        }
        let stamp = match mare_store::stamp(&path) {
            Ok(v) => v,
            Err(_) => {
                incomplete = true;
                continue;
            }
        };
        let h = match mare_store::bounded::header(&path, || cancel.is_cancelled()) {
            Ok(v) => v,
            Err(_) if cancel.is_cancelled() => return Err(super::query_err("QUERY_CANCELLED: query cancelled")),
            Err(_) => {
                incomplete = true;
                continue;
            }
        };
        if h["cwd"].as_str() != Some(scope.cwd.as_str()) {
            continue;
        }
        let Ok(Some(header)) = sanitize_header(&h) else {
            if !h["parent_session_id"].is_null() {
                incomplete = true;
            }
            continue;
        };
        sources.push((path, stamp, header.clone()));
        headers.push(header);
    }
    Ok(Catalog { headers, sources, incomplete })
}

pub(super) fn validate_live(
    scope: &SourceScope,
    live_inputs: &[LiveSessionInput],
    apply_step_boundary: bool,
) -> Vec<LiveRecord> {
    let mut out = Vec::new();
    for item in live_inputs {
        if item.session_epoch.is_empty() {
            continue;
        }
        let p = Path::new(&item.path);
        if scope.policy.as_ref().is_some_and(|pol| !pol.can_read(p)) {
            continue;
        }
        let Some(first) = item.entries.first() else {
            continue;
        };
        if mare_store::check_header(first).is_err() || first["cwd"].as_str() != Some(scope.cwd.as_str()) {
            continue;
        }
        let Ok(Some(header)) = sanitize_header(first) else {
            continue;
        };
        let id = header["session_id"].as_str().unwrap_or_default().to_owned();
        let mut entries = item.entries.clone();
        if apply_step_boundary && let Some(bound) = item.step_boundary {
            entries.truncate(bound.max(1).min(entries.len()));
        }
        out.push(LiveRecord { id, header, entries, session_epoch: item.session_epoch.clone() });
    }
    out
}

pub(super) fn overlay_headers(disk_headers: &[Value], live: &[LiveRecord]) -> Vec<Value> {
    let mut out = Vec::with_capacity(disk_headers.len() + live.len());
    let live_ids: HashSet<&str> = live.iter().map(|l| l.id.as_str()).collect();
    for h in disk_headers {
        if let Some(id) = h["session_id"].as_str()
            && !live_ids.contains(id)
        {
            out.push(h.clone());
        }
    }
    for l in live {
        out.push(l.header.clone());
    }
    out
}

pub(super) fn read_source_entries(
    source: &SourceKind,
    cancel: &CancellationToken,
) -> std::result::Result<(Vec<Value>, u64), ReadErr> {
    match source {
        SourceKind::Live(entries) => {
            let bytes = serde_json::to_vec(entries).map(|b| b.len() as u64).unwrap_or(0);
            Ok((entries.clone(), bytes))
        }
        SourceKind::Disk(path, stamp) => {
            if stamp.len > mare_store::bounded::MAX_BYTES {
                return Err(ReadErr::Budget);
            }
            match mare_store::bounded::read_with_cancel(path, stamp.len, || cancel.is_cancelled()) {
                Ok(v) => Ok((v, stamp.len)),
                Err(_) if cancel.is_cancelled() => Err(ReadErr::Cancelled),
                Err(_) => Err(ReadErr::Incomplete),
            }
        }
    }
}

pub(super) fn corpus_with_live(
    scope: &SourceScope,
    cancel: &CancellationToken,
    target: Option<&str>,
    filters: Option<&Filters>,
    live_inputs: &[LiveSessionInput],
) -> mlua::Result<Corpus> {
    let Catalog { headers: disk_headers, sources, mut incomplete } = catalog(scope, cancel)?;
    let mut predicate = filters.cloned().unwrap_or_default();
    predicate.archived = None;
    predicate.validate()?;

    let (wants_live, wants_persisted) = availability_flags(&predicate);
    let live_records = if wants_live { validate_live(scope, live_inputs, true) } else { Vec::new() };
    let headers = overlay_headers(&disk_headers, &live_records);

    let (mut budget, mut sessions, mut logical, mut live_ids) =
        (TOTAL_BYTES, Vec::new(), HashMap::new(), HashSet::new());

    for rec in live_records {
        if cancel.is_cancelled() {
            return Err(super::query_err("QUERY_CANCELLED: query cancelled"));
        }
        if target.is_some_and(|id| rec.id != id)
            || !predicate.accepts_with_availability(std::slice::from_ref(&rec.header), &headers, true)?
        {
            continue;
        }
        let live_gen = digest(&rec.entries);
        logical.insert(rec.id.clone(), live_gen);
        live_ids.insert(rec.id);
        sessions.push(rec.entries);
    }

    if wants_persisted {
        for (path, stamp, h) in &sources {
            if cancel.is_cancelled() {
                return Err(super::query_err("QUERY_CANCELLED: query cancelled"));
            }
            let id = h["session_id"].as_str().unwrap_or_default().to_owned();
            if live_ids.contains(&id) {
                continue;
            }
            if target.is_some_and(|t| id != t)
                || !predicate.accepts_with_availability(std::slice::from_ref(h), &headers, false)?
            {
                continue;
            }
            if stamp.len > mare_store::bounded::MAX_BYTES || stamp.len > budget {
                return Err(super::query_err(
                    "QUERY_BUDGET_EXCEEDED: raw snapshot budget exceeded; narrow the session/time filters",
                ));
            }
            budget -= stamp.len;
            let entries = match mare_store::bounded::read_with_cancel(path, stamp.len, || cancel.is_cancelled()) {
                Ok(v) => v,
                Err(_) if cancel.is_cancelled() => return Err(super::query_err("QUERY_CANCELLED: query cancelled")),
                Err(_) => {
                    incomplete = true;
                    continue;
                }
            };
            validate_source_identity(&entries, h)?;
            let generation = digest(&entries);
            if !record_logical_generation(&mut logical, id, generation)? {
                continue;
            }
            sessions.push(entries);
        }
    }
    ensure_stamps_unchanged(&sources)?;
    Ok(Corpus { sessions, live_ids, incomplete })
}

#[cfg(test)]
pub(super) fn corpus(
    scope: &SourceScope,
    cancel: &CancellationToken,
    target: Option<&str>,
    filters: Option<&Filters>,
) -> mlua::Result<Corpus> {
    corpus_with_live(scope, cancel, target, filters, &[])
}
