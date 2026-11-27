//! Deterministic surface projection and event-lineage tracing over a session's log entries.
use mlua::Result;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Surface {
    Current,
    Shadowed,
    LogOnly,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Shadowed => "shadowed",
            Self::LogOnly => "log_only",
        }
    }
}

#[derive(Debug)]
pub struct Projection {
    pub surfaces: Vec<Surface>,
    pub replaced_by: Vec<Option<usize>>,
    pub replaced_event_seqs: HashMap<usize, Vec<usize>>,
    pub source_event_seqs: Vec<Vec<usize>>,
    pub derived_event_seqs: Vec<Vec<usize>>,
}

impl Projection {
    pub fn replacement_chain(&self, mut seq: usize) -> Vec<usize> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        while let Some(Some(next)) = self.replaced_by.get(seq).copied() {
            if !seen.insert(next) {
                break;
            }
            chain.push(next);
            seq = next;
        }
        chain
    }
}

fn is_non_tree_type(kind: Option<&str>) -> bool {
    matches!(kind, Some("session" | "label" | "name" | "leaf" | "state" | "model" | "prune" | "tool_state" | "custom"))
}

/// A semantic item and the journal entry that introduced it. Control records are not items.
#[derive(Clone, Debug)]
pub(super) struct SemanticItem {
    pub seq: usize,
    pub value: Value,
}

pub(super) fn semantic_items(entries: &[Value]) -> Vec<SemanticItem> {
    let mut results = HashMap::new();
    for entry in entries {
        if entry["type"] == "message"
            && entry["message"]["role"] == "tool_result"
            && let Some(call_id) = entry["message"]["tool_call_id"].as_str()
        {
            results.entry(call_id).or_insert(entry);
        }
    }
    let mut seen_calls = HashSet::new();
    let mut items = Vec::new();
    for (seq, entry) in entries.iter().enumerate() {
        if entry["type"] != "message" {
            continue;
        }
        let Some(entry_id) = entry["entry_id"].as_str().filter(|id| !id.is_empty()) else { continue };
        let message = &entry["message"];
        let Some(role) = message["role"].as_str() else { continue };
        if role == "tool_result" {
            continue;
        }
        let kind = match role {
            "user" => "user_message",
            "assistant" => "assistant_message",
            _ => "custom",
        };
        let mut message_item = json!({ "item_id": entry_id, "entry_id": entry_id, "type": kind, "message": message });
        if let Some(parent) = entry["parent_entry_id"].as_str() {
            message_item["parent_entry_id"] = json!(parent);
        }
        items.push(SemanticItem { seq, value: message_item });
        if role != "assistant" {
            continue;
        }
        let calls = message["tool_calls"].as_array().into_iter().flatten().chain(
            message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "tool_call" && block["hosted"].as_bool() != Some(true)),
        );
        for call in calls {
            let Some(call_id) = call["id"].as_str().filter(|id| !id.is_empty()) else { continue };
            if !seen_calls.insert(call_id.to_owned()) {
                continue;
            }
            let mut item = json!({
                "item_id": call_id, "entry_id": entry_id, "type": "tool_call", "tool_call_id": call_id,
            });
            if let Some(name) = call["name"].as_str().filter(|name| !name.is_empty()) {
                item["name"] = json!(name);
            }
            if let Some(provider_call_id) = call["provider_call_id"].as_str().filter(|id| !id.is_empty()) {
                item["provider_call_id"] = json!(provider_call_id);
            }
            if !call["arguments"].is_null() {
                item["arguments"] = call["arguments"].clone();
            }
            if let Some(result) = results.get(call_id) {
                if let Some(id) = result["entry_id"].as_str() {
                    item["result_entry_id"] = json!(id);
                }
                item["message"] = result["message"].clone();
            }
            items.push(SemanticItem { seq, value: item });
        }
    }
    items
}

pub(super) fn install(lua: &mlua::Lua, store: &mlua::Table) -> Result<()> {
    store.set(
        "project_items",
        lua.create_function(|lua, value: mlua::Value| {
            let entries =
                mare_shared::lua::strict_lua_to_json_bounded(lua, value, mare_shared::lua::HISTORY_LOSSLESS_MAX_NODES)?;
            let entries = entries.as_array().ok_or_else(|| mlua::Error::external("entries must be an array"))?;
            let items: Vec<Value> = semantic_items(entries).into_iter().map(|item| item.value).collect();
            mare_shared::lua::json_to_lua_lossless_bounded(
                lua,
                &Value::Array(items),
                mare_shared::lua::HISTORY_LOSSLESS_MAX_NODES,
            )
        })?,
    )
}

pub fn project(entries: &[Value]) -> Result<Projection> {
    let len = entries.len();
    let mut surfaces = vec![Surface::LogOnly; len];
    let mut replaced_by = vec![None; len];
    let mut replaced_event_seqs: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut source_event_seqs = vec![Vec::new(); len];
    let mut derived_event_seqs = vec![Vec::new(); len];
    if len <= 1 {
        return Ok(Projection { surfaces, replaced_by, replaced_event_seqs, source_event_seqs, derived_event_seqs });
    }

    let mut id_to_seq: HashMap<&str, usize> = HashMap::new();
    let mut call_id_to_seq: HashMap<&str, usize> = HashMap::new();
    let mut leaf: Option<&str> = None;

    for (seq, entry) in entries.iter().enumerate().skip(1) {
        if let Some(id) = entry["entry_id"].as_str() {
            id_to_seq.insert(id, seq);
            if !is_non_tree_type(entry["type"].as_str()) {
                leaf = Some(id);
            }
        }
        if entry["type"] == "leaf" {
            leaf = entry["target_entry_id"].as_str();
        }
        if entry["type"] == "message" && entry["message"]["role"] == "assistant" {
            if let Some(calls) = entry["message"]["tool_calls"].as_array() {
                for call in calls {
                    if let Some(cid) = call["id"].as_str() {
                        call_id_to_seq.insert(cid, seq);
                    }
                }
            }
            if let Some(blocks) = entry["message"]["content"].as_array() {
                for block in blocks {
                    let kind = block["type"].as_str();
                    if matches!(kind, Some("tool_call" | "tool_use"))
                        && let Some(cid) = block["id"].as_str()
                    {
                        call_id_to_seq.insert(cid, seq);
                    }
                }
            }
        }
    }

    let mut active_path = Vec::new();
    let mut seen = HashSet::new();
    let mut cur = leaf.and_then(|id| id_to_seq.get(id).copied());
    while let Some(seq) = cur {
        if !seen.insert(seq) {
            return Err(crate::session_query::query_err("QUERY_INVALID_LINEAGE: session tree contains a parent cycle"));
        }
        active_path.push(seq);
        cur = entries[seq]["parent_entry_id"].as_str().and_then(|pid| id_to_seq.get(pid).copied());
    }
    active_path.reverse();

    let mut active: Vec<usize> = Vec::new();
    for seq in active_path {
        if entries[seq]["type"] == "compaction" {
            let keep_pos = entries[seq]["first_kept_entry_id"]
                .as_str()
                .and_then(|fk| active.iter().position(|&s| entries[s]["entry_id"].as_str() == Some(fk)))
                .unwrap_or(active.len());
            let drained: Vec<usize> = active.drain(..keep_pos).collect();
            for &s in &drained {
                replaced_by[s] = Some(seq);
            }
            if !drained.is_empty() {
                replaced_event_seqs.insert(seq, drained);
            }
            active.push(seq);
        } else {
            active.push(seq);
        }
    }
    for seq in active {
        surfaces[seq] = Surface::Current;
    }
    for (seq, rep) in replaced_by.iter().enumerate() {
        if rep.is_some() {
            surfaces[seq] = Surface::Shadowed;
        }
    }

    for seq in 1..len {
        let entry = &entries[seq];
        let mut sources = BTreeSet::new();
        let add_seq = |s: usize, set: &mut BTreeSet<usize>| {
            if s >= 1 && s < seq {
                set.insert(s);
            }
        };
        if let Some(arr) = entry["source_event_seqs"].as_array() {
            for v in arr {
                if let Some(s) = v.as_u64().and_then(|n| usize::try_from(n).ok()) {
                    add_seq(s, &mut sources);
                }
            }
        }
        match entry["type"].as_str() {
            Some("compaction") => {
                if let Some(rep) = replaced_event_seqs.get(&seq) {
                    for &s in rep {
                        add_seq(s, &mut sources);
                    }
                }
                for key in ["from_entry_id", "to_entry_id"] {
                    if let Some(id) = entry["range"][key].as_str()
                        && let Some(&s) = id_to_seq.get(id)
                    {
                        add_seq(s, &mut sources);
                    }
                }
            }
            Some("branch_summary") => {
                if let Some(id) = entry["from_entry_id"].as_str()
                    && let Some(&s) = id_to_seq.get(id)
                {
                    add_seq(s, &mut sources);
                }
            }
            Some("prune") => {
                if let Some(targets) = entry["target_entry_ids"].as_object() {
                    for id in targets.keys() {
                        if let Some(&s) = id_to_seq.get(id.as_str()) {
                            add_seq(s, &mut sources);
                        }
                    }
                }
            }
            Some("label") => {
                if let Some(id) = entry["target_entry_id"].as_str()
                    && let Some(&s) = id_to_seq.get(id)
                {
                    add_seq(s, &mut sources);
                }
            }
            Some("message") => {
                if matches!(entry["message"]["role"].as_str(), Some("tool_result" | "toolResult" | "tool")) {
                    if let Some(cid) = entry["message"]["tool_call_id"].as_str()
                        && let Some(&s) = call_id_to_seq.get(cid)
                    {
                        add_seq(s, &mut sources);
                    }
                    if let Some(blocks) = entry["message"]["content"].as_array() {
                        for block in blocks {
                            for key in ["tool_call_id", "tool_use_id"] {
                                if let Some(cid) = block[key].as_str()
                                    && let Some(&s) = call_id_to_seq.get(cid)
                                {
                                    add_seq(s, &mut sources);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        let vec_sources: Vec<usize> = sources.into_iter().collect();
        for &src in &vec_sources {
            derived_event_seqs[src].push(seq);
        }
        source_event_seqs[seq] = vec_sources;
    }

    Ok(Projection { surfaces, replaced_by, replaced_event_seqs, source_event_seqs, derived_event_seqs })
}

pub fn trace_event(entries: &[Value], seq: usize, event_id: &str, summary: &str) -> Result<Value> {
    if seq == 0 || seq >= entries.len() {
        return Err(crate::session_query::query_err("QUERY_NOT_FOUND: event unavailable in this workspace"));
    }
    let proj = project(entries)?;
    let entry = &entries[seq];
    let mut out = json!({
        "session_id": entries[0]["session_id"],
        "event_id": event_id,
        "seq": seq,
        "entry_id": entry.get("entry_id").and_then(Value::as_str),
        "type": entry["type"].as_str().unwrap_or("unknown"),
        "role": entry.get("message").and_then(|m| m.get("role")).and_then(Value::as_str),
        "surface": proj.surfaces[seq].as_str(),
        "replaced_by": proj.replaced_by[seq],
        "replacement_chain": proj.replacement_chain(seq),
        "replaced_event_seqs": proj.replaced_event_seqs.get(&seq).cloned().unwrap_or_default(),
        "source_event_seqs": proj.source_event_seqs[seq],
        "derived_event_seqs": proj.derived_event_seqs[seq],
        "summary": summary,
    });
    if let Some(ms) = entry["created_at_ms"].as_i64() {
        out["created_at_ms"] = json!(ms);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_compaction_chains_abandoned_branches_and_tool_links() {
        let entries = vec![
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"s1","cwd":"/work"}),
            json!({"type":"message","entry_id":"u1","message":{"role":"user","content":"first"}}),
            json!({"type":"message","entry_id":"a1","parent_entry_id":"u1","message":{"role":"assistant","tool_calls":[{"id":"c1","name":"read"}]}}),
            json!({"type":"message","entry_id":"t1","parent_entry_id":"a1","message":{"role":"tool_result","tool_call_id":"c1","content":"ok"}}),
            json!({"type":"message","entry_id":"ab1","parent_entry_id":"u1","message":{"role":"user","content":"abandoned"}}),
            json!({"type":"leaf","target_entry_id":"t1"}),
            json!({"type":"compaction","entry_id":"cmp1","parent_entry_id":"t1","summary":"sum1","first_kept_entry_id":"t1"}),
            json!({"type":"branch_summary","entry_id":"bs1","parent_entry_id":"cmp1","from_entry_id":"ab1","summary":"abandoned branch"}),
            json!({"type":"compaction","entry_id":"cmp2","parent_entry_id":"bs1","summary":"sum2"}),
            json!({"type":"message","entry_id":"u2","parent_entry_id":"cmp2","message":{"role":"user","content":"latest"}}),
            json!({"type":"name","name":"Session Title"}),
        ];
        let proj = project(&entries).unwrap();
        assert_eq!(proj.surfaces[1], Surface::Shadowed);
        assert_eq!(proj.surfaces[2], Surface::Shadowed);
        assert_eq!(proj.surfaces[3], Surface::Shadowed); // shadowed by cmp2
        assert_eq!(proj.surfaces[4], Surface::LogOnly); // abandoned branch
        assert_eq!(proj.surfaces[5], Surface::LogOnly); // leaf marker
        assert_eq!(proj.surfaces[6], Surface::Shadowed); // cmp1 shadowed by cmp2
        assert_eq!(proj.surfaces[7], Surface::Shadowed); // bs1 shadowed by cmp2
        assert_eq!(proj.surfaces[8], Surface::Current); // cmp2
        assert_eq!(proj.surfaces[9], Surface::Current); // u2
        assert_eq!(proj.surfaces[10], Surface::LogOnly); // name
        assert_eq!(proj.replacement_chain(1), vec![6, 8]);
        assert_eq!(proj.source_event_seqs[3], vec![2]);
        assert_eq!(proj.derived_event_seqs[2], vec![3, 6]);
        assert_eq!(proj.source_event_seqs[7], vec![4]);
        assert_eq!(proj.derived_event_seqs[4], vec![7]);
    }

    #[test]
    fn rejects_parent_cycle() {
        let entries = vec![
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"cyc","cwd":"/work"}),
            json!({"type":"message","entry_id":"a","parent_entry_id":"b","message":{"role":"user","content":"a"}}),
            json!({"type":"message","entry_id":"b","parent_entry_id":"a","message":{"role":"assistant","content":"b"}}),
        ];
        assert!(project(&entries).unwrap_err().to_string().contains("QUERY_INVALID_LINEAGE"));
    }
}
