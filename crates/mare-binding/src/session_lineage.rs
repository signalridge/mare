//! Read-only lineage of authorized native session headers. No writer or serializer.
use crate::session_query::query_err;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use tokio_util::sync::CancellationToken;

fn node(header: &Value) -> Value {
    let mut out = json!({"session_id": header["session_id"], "title_available": false});
    if let Some(ms) = header["created_at_ms"].as_i64() {
        out["created_at_ms"] = json!(ms);
    }
    out
}
/// Parent ids are never copied into output; an absent/denied parent is an anonymous boundary.
pub(super) fn trace(headers: &[Value], target: &str, cancel: &CancellationToken) -> mlua::Result<Value> {
    let mut by_id = BTreeMap::new();
    for header in headers {
        let id = header["session_id"].as_str().unwrap_or_default();
        if let Some(old) = by_id.insert(id, header)
            && old != header
        {
            return Err(query_err("QUERY_SOURCE_CONFLICT: conflicting logical session headers"));
        }
    }
    let target_header = *by_id
        .get(target)
        .ok_or_else(|| query_err("QUERY_SESSION_UNAVAILABLE: session unavailable in this workspace"))?;
    let mut seen = HashSet::from([target]);
    let mut ancestors = Vec::new();
    let mut at = target_header;
    let mut boundary = false;
    while let Some(parent) = at["parent_session_id"].as_str() {
        if cancel.is_cancelled() {
            return Err(query_err("QUERY_CANCELLED: query cancelled"));
        }
        if !seen.insert(parent) {
            return Err(query_err("QUERY_INVALID_LINEAGE: session ancestry contains a cycle"));
        }
        if ancestors.len() == 1000 {
            return Err(query_err("QUERY_BUDGET_EXCEEDED: ancestry result limit exceeded"));
        }
        let Some(next) = by_id.get(parent) else {
            boundary = true;
            break;
        };
        ancestors.push(node(next));
        at = next;
    }
    let mut children: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for header in by_id.values() {
        if let Some(parent) = header["parent_session_id"].as_str() {
            children.entry(parent).or_default().push(header);
        }
    }
    for list in children.values_mut() {
        list.sort_by(|a, b| {
            a["created_at_ms"]
                .as_i64()
                .unwrap_or(0)
                .cmp(&b["created_at_ms"].as_i64().unwrap_or(0))
                .then_with(|| a["session_id"].as_str().cmp(&b["session_id"].as_str()))
        });
    }
    let mut count = 0;
    fn descend(
        id: &str,
        children: &BTreeMap<&str, Vec<&Value>>,
        depth: usize,
        count: &mut usize,
        cancel: &CancellationToken,
    ) -> mlua::Result<Vec<Value>> {
        if cancel.is_cancelled() {
            return Err(query_err("QUERY_CANCELLED: query cancelled"));
        }
        let mut out = Vec::new();
        for child in children.get(id).into_iter().flatten() {
            *count += 1;
            if depth >= 32 || *count > 1000 {
                return Err(query_err("QUERY_BUDGET_EXCEEDED: lineage result depth/count limit exceeded"));
            }
            let id = child["session_id"].as_str().unwrap_or_default();
            out.push(json!({"session":node(child),"descendants":descend(id,children,depth+1,count,cancel)?}));
        }
        Ok(out)
    }
    let descendants = descend(target, &children, 0, &mut count, cancel)?;
    let root = (!boundary).then(|| node(at));
    Ok(
        json!({"target":node(target_header),"ancestors":ancestors,"descendants":descendants,"complete":!boundary,"root":root,"parent_boundary":boundary,"metadata_only":true,"scope":"authorized-exact-cwd"}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_header_edges_and_private_boundaries() {
        let headers = vec![
            json!({"session_id":"root","created_at_ms":1}),
            json!({"session_id":"child","parent_session_id":"root","created_at_ms":2}),
            json!({"session_id":"leaf","parent_session_id":"child","created_at_ms":3}),
        ];
        let result = trace(&headers, "child", &CancellationToken::new()).unwrap();
        assert_eq!(result["ancestors"][0]["session_id"], "root");
        assert_eq!(result["descendants"][0]["session"]["session_id"], "leaf");
        assert_eq!(result["complete"], true);
        let result = trace(
            &[json!({"session_id":"allowed","parent_session_id":"secret-denied-parent"})],
            "allowed",
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(result["parent_boundary"], true);
        assert!(!result.to_string().contains("secret-denied-parent"));
    }
    #[test]
    fn cycles_fail_explicitly() {
        assert!(
            trace(
                &[json!({"session_id":"a","parent_session_id":"b"}), json!({"session_id":"b","parent_session_id":"a"})],
                "a",
                &CancellationToken::new()
            )
            .unwrap_err()
            .to_string()
            .contains("QUERY_INVALID_LINEAGE")
        );
    }
}
