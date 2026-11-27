//! Policy-bound, cwd-scoped, generation-checked history. Never consults the host index.
mod corpus;
mod reader;
mod search;
mod trace;

#[cfg(test)]
mod tests;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use mare_shared::lua::lua_to_json;
use mlua::{Lua, Table};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

use crate::session_filters::{EventFilters, SessionFilters as Filters};

pub(super) const CATALOG_LIMIT: usize = 10_000;
pub(super) const TOTAL_BYTES: u64 = 32 * 1024 * 1024;
pub(super) const INLINE_EVENT_LIMIT: usize = 65_536;
pub(super) const PAGE: usize = 20;

pub(super) struct CancelOnDrop(pub CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Search {
    #[serde(default)]
    pub query: String,
    pub title_query: Option<String>,
    pub sort_key: Option<String>,
    pub sort_direction: Option<String>,
    pub filters: Option<Filters>,
    pub event_filters: Option<EventFilters>,
    pub session_id: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    pub step_bound: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Read {
    pub event_id: String,
    pub before: Option<usize>,
    pub after: Option<usize>,
    pub offset: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadItem {
    pub session_id: String,
    pub item_id: String,
    pub before: Option<usize>,
    pub after: Option<usize>,
    pub offset: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TraceEventArgs {
    pub session_id: Option<String>,
    pub event_id: Option<String>,
    pub seq: Option<usize>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LiveSessionInput {
    pub path: String,
    pub entries: Vec<Value>,
    pub session_epoch: String,
    #[serde(default)]
    pub step_boundary: Option<usize>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Position {
    pub generation: String,
    pub session_id: String,
    pub seq: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cursor {
    pub generation: String,
    pub identity: String,
    #[serde(default)]
    pub batch_start: usize,
    pub offset: usize,
}

pub(super) struct Corpus {
    pub sessions: Vec<Vec<Value>>,
    pub live_ids: HashSet<String>,
    pub incomplete: bool,
}

#[derive(Debug)]
pub(super) struct SourceScope {
    pub cwd: String,
    pub owner: String,
    pub dir: PathBuf,
    pub policy: Option<crate::ossandbox::Policy>,
}

pub(super) fn encode<T: Serialize>(v: &T) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).expect("history token"))
}

pub(super) fn coded_err(code: &'static str, message: impl Into<String>) -> mlua::Error {
    mlua::Error::external(mare_shared::errors::Coded { code, message: message.into() })
}

pub(super) fn parse_args<T: serde::de::DeserializeOwned>(lua: &Lua, val: mlua::Value) -> mlua::Result<T> {
    let json = lua_to_json(lua, val)?;
    serde_path_to_error::deserialize(json).map_err(|e| coded_err("invalid_arguments", e.to_string()))
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(s: &str) -> mlua::Result<T> {
    if s.len() > 4096 {
        return Err(coded_err("invalid_arguments", "invalid history token"));
    }
    let b = URL_SAFE_NO_PAD.decode(s).map_err(|_| coded_err("invalid_arguments", "invalid history token"))?;
    serde_json::from_slice(&b).map_err(|_| coded_err("invalid_arguments", "invalid history token"))
}

pub(super) fn digest(v: &impl Serialize) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(serde_json::to_vec(v).expect("history digest")))
}

pub(super) fn scope(lua: &Lua) -> mlua::Result<SourceScope> {
    let (cwd, active, has_exec_ctx) = if let Some(context) = mare_shared::exec_ctx::current(lua) {
        let cwd: String = context.get("cwd")?;
        let active = crate::exec_ctx::active(lua)?;
        let cwd = active.as_ref().map_or(cwd, |a| a.cwd.to_string_lossy().into_owned());
        (cwd, active, true)
    } else {
        let (cwd_path, active) = crate::exec_ctx::host_query_scope(lua)?;
        (cwd_path.to_string_lossy().into_owned(), active, false)
    };
    let dir = mare_shared::paths::sessions_dir().join(mare_store::cwd_slug(Path::new(&cwd)));
    if has_exec_ctx {
        crate::exec_ctx::check_process_path(lua, "read", &dir.to_string_lossy())?;
    } else if let Some(ref a) = active {
        crate::exec_ctx::check_active_path(lua, a, "read", &dir.to_string_lossy())?;
    }
    let owner = mare_shared::txn_scope::current_caller_owner().unwrap_or_else(|| "host".to_string());
    Ok(SourceScope { cwd, owner, dir, policy: active.map(|a| a.policy) })
}

pub(super) fn parse_live(lua: &Lua, opts: &Option<Table>) -> mlua::Result<Vec<LiveSessionInput>> {
    let Some(opts) = opts else {
        return Ok(Vec::new());
    };
    let val: mlua::Value = opts.get("live_sessions")?;
    if matches!(val, mlua::Value::Nil) {
        return Ok(Vec::new());
    }
    parse_args(lua, val)
}

pub(super) fn text(entry: &Value) -> String {
    match entry["type"].as_str() {
        Some("message") => match &entry["message"]["content"] {
            Value::String(s) => s.clone(),
            Value::Array(a) => a
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        },
        Some("compaction" | "branch_summary") => entry["summary"].as_str().unwrap_or_default().to_owned(),
        _ => String::new(),
    }
}

pub(super) fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub(super) fn title(entries: &[Value]) -> String {
    entries
        .iter()
        .rev()
        .find(|e| e["type"] == "name")
        .and_then(|e| e["name"].as_str())
        .map(|s| clip(s, 160))
        .unwrap_or_default()
}

pub(super) fn session_timestamps(entries: &[Value]) -> (i64, i64) {
    let created = entries.first().and_then(|e| e["created_at_ms"].as_i64()).unwrap_or(0);
    let updated = entries.iter().skip(1).filter_map(|e| e["created_at_ms"].as_i64()).fold(created, i64::max);
    (created, updated)
}

/// Install policy-bound session query primitives on `mare.store`.
pub fn install(lua: &Lua, store: &Table) -> mlua::Result<()> {
    search::install(lua, store)?;
    trace::install(lua, store)?;
    reader::install(lua, store)?;
    Ok(())
}

/// A query failure: its `QUERY_*` text says what failed, and it carries the vocabulary code
/// callers act on.
pub(crate) fn query_err(message: impl Into<String>) -> mlua::Error {
    let message = message.into();
    let kind = message.split(':').next().unwrap_or_default();
    let code = match kind {
        "QUERY_CANCELLED" => "cancelled",
        "QUERY_INVALID_CURSOR" | "QUERY_INVALID_FILTER" | "QUERY_INVALID_QUERY" | "QUERY_EVENT_TOO_LARGE" => {
            "invalid_arguments"
        }
        "QUERY_NOT_FOUND"
        | "QUERY_SESSION_UNAVAILABLE"
        | "QUERY_STORAGE_UNAVAILABLE"
        | "QUERY_CAPABILITY_UNAVAILABLE" => "unavailable",
        "QUERY_SOURCE_CHANGED" | "QUERY_SOURCE_CONFLICT" => "stale_session",
        "QUERY_INVALID_LINEAGE" => "corrupt",
        "QUERY_INDEX_FAILED" => "io",
        "QUERY_UNAUTHORIZED" => "denied",
        "QUERY_BUDGET_EXCEEDED" => "overloaded",
        _ => return mlua::Error::runtime(message),
    };
    coded_err(code, message)
}
