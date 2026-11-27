//! `mare.ui.completion`: extensible completion source registry.
//!
//! Sources declare `{ name, triggers, complete, enabled?, min_prefix?, score_offset?, timeout_ms? }`
//! and return an idempotent `release()` callback. `app/input/completion.rs` reads source metadata
//! from `UiShared::completion_sources`, runs matching sources concurrently with `timeout_ms`,
//! discards stale `seq` results, sorts by descending effective score, and retracts candidates when
//! `release()` fires.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::future::join_all;
use mlua::{Function, Lua, MultiValue, Table, Value};

use mare_shared::lua::call_async;
use mare_shared::ui::{CompletionSourceMeta, UiCmd, UiShared};

use super::non_neg_int;

const DEFAULT_TIMEOUT_MS: u64 = 1000;
const CALLBACKS_REG_KEY: &str = "mare.ui.completion.callbacks";

fn parse_triggers(v: Value) -> mlua::Result<Vec<String>> {
    match v {
        Value::Nil => Ok(Vec::new()),
        Value::Table(t) => {
            let mut out = Vec::new();
            for item in t.sequence_values::<Value>() {
                let Value::String(s) = item? else {
                    return Err(mlua::Error::runtime("mare.ui.completion.register: triggers entries must be strings"));
                };
                let s = s.to_str()?.to_string();
                if s.is_empty() {
                    return Err(mlua::Error::runtime(
                        "mare.ui.completion.register: triggers entries must be non-empty",
                    ));
                }
                out.push(s);
            }
            Ok(out)
        }
        _ => Err(mlua::Error::runtime("mare.ui.completion.register: triggers must be an array of strings")),
    }
}

fn non_neg_usize(v: Value, field: &str) -> mlua::Result<Option<usize>> {
    if matches!(v, Value::Nil) {
        return Ok(None);
    }
    non_neg_int(v, &format!("mare.ui.completion.register: {field}")).map(Some)
}

fn score_offset_i32(v: Value) -> mlua::Result<i32> {
    match v {
        Value::Nil => Ok(0),
        Value::Integer(n) => {
            i32::try_from(n).map_err(|_| mlua::Error::runtime("mare.ui.completion.register: score_offset out of range"))
        }
        Value::Number(n) if n.is_finite() => Ok(n.round() as i32),
        _ => Err(mlua::Error::runtime("mare.ui.completion.register: score_offset must be a number")),
    }
}

/// Whether `source` matches `token` (either manual `force == true` or trigger + `min_prefix`).
pub fn source_matches(meta: &CompletionSourceMeta, token: &str, force: bool) -> bool {
    if force {
        return true;
    }
    meta.triggers
        .iter()
        .any(|trig| token.strip_prefix(trig.as_str()).is_some_and(|rest| rest.chars().count() >= meta.min_prefix))
}

/// Evaluate one registered completion source with `timeout_ms` and return `(score, item)` pairs.
pub async fn run_source(
    lua: &Lua,
    shared: &Arc<UiShared>,
    meta: &CompletionSourceMeta,
    ev: Value,
) -> Vec<(f64, Table)> {
    let Ok(callbacks) = lua.named_registry_value::<Table>(CALLBACKS_REG_KEY) else {
        return Vec::new();
    };
    let Ok(Value::Table(entry)) = callbacks.get::<Value>(meta.id) else {
        return Vec::new();
    };
    let budget = Duration::from_millis(meta.timeout_ms.max(1));
    let timed = tokio::time::timeout(budget, async {
        match entry.get::<Value>("enabled") {
            Ok(Value::Boolean(false)) => return None,
            Ok(Value::Function(enabled_fn)) => {
                let ok = match call_async::<Value>(lua, &enabled_fn, ev.clone()).await {
                    Ok(v) => !matches!(v, Value::Nil | Value::Boolean(false)),
                    Err(_) => false,
                };
                if !ok {
                    return None;
                }
            }
            _ => {}
        }
        let Ok(complete_fn) = entry.get::<Function>("complete") else {
            return None;
        };
        call_async::<Value>(lua, &complete_fn, ev).await.ok()
    })
    .await;
    let Ok(Some(Value::Table(list))) = timed else {
        return Vec::new();
    };
    // Drop candidates immediately if the source was released while in-flight.
    if !shared.completion_sources.read().unwrap().iter().any(|s| s.id == meta.id) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for item in list.sequence_values::<Value>().flatten() {
        let Value::Table(t) = item else { continue };
        let item_score = match t.get::<Value>("score") {
            Ok(Value::Integer(n)) => n as f64,
            Ok(Value::Number(n)) if n.is_finite() => n,
            _ => 0.0,
        };
        let _ = t.set("source", meta.name.as_str());
        let _ = t.set("source_id", meta.id);
        out.push((item_score + f64::from(meta.score_offset), t));
    }
    out
}

/// Filter matching registered sources, run them concurrently, and return ranked `(source_id, item)`.
pub async fn collect_ranked(
    lua: &Lua,
    shared: &Arc<UiShared>,
    ev: &Value,
    token: &str,
    force: bool,
) -> Vec<(u64, Table)> {
    let matching: Vec<CompletionSourceMeta> =
        shared.completion_sources.read().unwrap().iter().filter(|m| source_matches(m, token, force)).cloned().collect();
    if matching.is_empty() {
        return Vec::new();
    }
    let futs = matching.iter().map(|m| async move {
        let entries = run_source(lua, shared, m, ev.clone()).await;
        (m.id, entries)
    });
    let per_source = join_all(futs).await;
    let mut ranked: Vec<(f64, usize, u64, Table)> = Vec::new();
    for (source_id, list) in per_source {
        for (score, tbl) in list {
            ranked.push((score, ranked.len(), source_id, tbl));
        }
    }
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked.into_iter().map(|(_, _, sid, tbl)| (sid, tbl)).collect()
}

pub(super) fn install(lua: &Lua, ui: &Table, shared: &Arc<UiShared>) -> mlua::Result<()> {
    let callbacks = lua.create_table()?;
    lua.set_named_registry_value(CALLBACKS_REG_KEY, callbacks)?;

    let comp = lua.create_table()?;

    let s = shared.clone();
    comp.set(
        "register",
        lua.create_function(move |lua, spec: Table| {
            let name: String = spec
                .get::<Option<String>>("name")?
                .filter(|n| !n.trim().is_empty())
                .ok_or_else(|| mlua::Error::runtime("mare.ui.completion.register: non-empty `name` is required"))?;
            let triggers = parse_triggers(spec.get::<Value>("triggers")?)?;
            let complete: Function = spec
                .get::<Option<Function>>("complete")?
                .ok_or_else(|| mlua::Error::runtime("mare.ui.completion.register: `complete` function is required"))?;
            let enabled: Value = spec.get("enabled")?;
            match &enabled {
                Value::Nil | Value::Boolean(_) | Value::Function(_) => {}
                _ => {
                    return Err(mlua::Error::runtime(
                        "mare.ui.completion.register: `enabled` must be a boolean or function",
                    ));
                }
            }
            let min_prefix = non_neg_usize(spec.get::<Value>("min_prefix")?, "min_prefix")?.unwrap_or(0);
            let score_offset = score_offset_i32(spec.get::<Value>("score_offset")?)?;
            let timeout_ms = non_neg_usize(spec.get::<Value>("timeout_ms")?, "timeout_ms")?
                .map(|n| n.max(1) as u64)
                .unwrap_or(DEFAULT_TIMEOUT_MS);

            let id = s.next_id.fetch_add(1, Ordering::Relaxed);
            let cb_tbl = lua.named_registry_value::<Table>(CALLBACKS_REG_KEY)?;
            let entry = lua.create_table()?;
            entry.set("complete", complete)?;
            entry.set("enabled", enabled)?;
            cb_tbl.set(id, entry)?;

            s.completion_sources.write().unwrap().push(CompletionSourceMeta {
                id,
                name,
                triggers,
                min_prefix,
                score_offset,
                timeout_ms,
            });

            let s_rel = s.clone();
            let released = Arc::new(AtomicBool::new(false));
            let release = lua.create_function(move |lua, ()| {
                if released.swap(true, Ordering::AcqRel) {
                    return Ok(());
                }
                s_rel.completion_sources.write().unwrap().retain(|src| src.id != id);
                if let Ok(cb_tbl) = lua.named_registry_value::<Table>(CALLBACKS_REG_KEY) {
                    let _ = cb_tbl.set(id, Value::Nil);
                }
                s_rel.send(UiCmd::CompletionSourceRemoved { id });
                Ok(())
            })?;
            Ok(release)
        })?,
    )?;

    // Collect merged completion items from all registered sources matching `ev` (for tests & RPC).
    let s = shared.clone();
    comp.set(
        "collect",
        lua.create_async_function(move |lua, ev: Table| {
            let s = s.clone();
            async move {
                let token = ev.get::<Option<String>>("token")?.unwrap_or_default();
                let force = ev.get::<Option<bool>>("force")?.unwrap_or(true);
                let ranked = collect_ranked(&lua, &s, &Value::Table(ev), &token, force).await;
                let out = lua.create_table()?;
                for (i, (_, tbl)) in ranked.into_iter().enumerate() {
                    out.set(i + 1, tbl)?;
                }
                Ok(Value::Table(out))
            }
        })?,
    )?;

    // Support `mare.ui.completion.register(...)` even without extra helpers.
    let s = shared.clone();
    comp.set(
        "list",
        lua.create_function(move |lua, ()| {
            let out = lua.create_table()?;
            for (i, m) in s.completion_sources.read().unwrap().iter().enumerate() {
                let row = lua.create_table()?;
                row.set("id", m.id)?;
                row.set("name", m.name.as_str())?;
                row.set("triggers", m.triggers.clone())?;
                row.set("min_prefix", m.min_prefix)?;
                row.set("score_offset", m.score_offset)?;
                row.set("timeout_ms", m.timeout_ms)?;
                out.set(i + 1, row)?;
            }
            Ok(MultiValue::from_vec(vec![Value::Table(out)]))
        })?,
    )?;

    ui.set("completion", comp)?;
    Ok(())
}
