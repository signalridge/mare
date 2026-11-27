//! RPC filter error coverage, following reference implementation.

use std::process::Stdio;

use super::*;
use crate::common::rpc::RpcClient;

#[test]
fn invalid_history_filters_return_invalid_arguments_over_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(home.join("config/init.lua"), "mare.setup { model = \"faux/echo\" }\n").unwrap();
    let script = dir.path().join("faux.json");
    std::fs::write(&script, "[]").unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["-C", work.to_str().unwrap(), "--yolo", "--rpc"])
        .env("MARE_HOME", &home)
        .env("MARE_FAUX_SCRIPT", &script)
        .stderr(Stdio::inherit());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));

    let valid = rpc.call_raw("session.search", json!({ "query": "needle" }), |_| json!(null));
    let cases = [
        (
            "legacy string filter",
            "session.search",
            json!({ "query": "needle", "filters": { "created_from_ms": "2026-07-24T10:00:00" } }),
        ),
        (
            "reversed time",
            "session.search",
            json!({ "query": "needle", "filters": { "created_from_ms": 1784937600000_i64, "created_to_ms": 1784851200000_i64 } }),
        ),
        ("empty ids", "session.search", json!({ "query": "needle", "filters": { "session_ids": [] } })),
        (
            "reversed sequence",
            "session.search_items",
            json!({ "session_id": "target", "query": "needle", "event_filters": { "seq_from": 2, "seq_to": 1 } }),
        ),
        (
            "empty event types",
            "session.search_items",
            json!({ "session_id": "target", "query": "needle", "event_filters": { "event_types": [] } }),
        ),
    ];
    let replies: Vec<_> =
        cases.into_iter().map(|(name, method, params)| (name, rpc.call_raw(method, params, |_| json!(null)))).collect();
    rpc.shutdown_and_wait();

    assert!(valid["result"].is_object(), "valid search should reach the provider: {valid}");
    for (name, reply) in replies {
        assert_eq!(reply["error"]["code"], json!(-32602), "{name}: {reply}");
        assert_eq!(reply["error"]["data"]["code"], json!("invalid_arguments"), "{name}: {reply}");
        if name != "legacy string filter" {
            assert!(
                reply["error"]["message"].as_str().unwrap_or("").contains("QUERY_INVALID_FILTER"),
                "{name}: {reply}"
            );
        }
    }
}

#[test]
fn history_ms_filter_bounds_and_inclusive_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(home.join("config/init.lua"), "mare.setup { model = \"faux/echo\" }\n").unwrap();
    let script = dir.path().join("faux.json");
    std::fs::write(&script, "[]").unwrap();

    let canon_work = dunce::canonicalize(&work).unwrap();
    let sdir = home.join("state/sessions").join(mare_store::cwd_slug(&canon_work));
    std::fs::create_dir_all(&sdir).unwrap();
    let t1: i64 = 1_700_000_000_000;
    let t2: i64 = 1_700_000_060_000;
    let write_sess = |stem: &str, sid: &str, created: i64, msg_ts: i64, text: &str| {
        let path = sdir.join(format!("{stem}.jsonl"));
        let lines = [
            json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": sid, "cwd": canon_work, "created_at_ms": created }),
            json!({ "type": "message", "entry_id": format!("{sid}-u1"), "created_at_ms": msg_ts, "message": { "role": "user", "content": text } }),
        ];
        std::fs::write(path, lines.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n") + "\n").unwrap();
    };
    write_sess("2023-11-14T22-13-20Z_s1", "sess-early", t1, t1 + 1000, "boundary-needle alpha");
    write_sess("2023-11-14T22-14-20Z_s2", "sess-late", t2, t2 + 1000, "boundary-needle beta");

    let mut cmd = mare_cmd();
    cmd.args(["-C", work.to_str().unwrap(), "--yolo", "--rpc"])
        .env("MARE_HOME", &home)
        .env("MARE_FAUX_SCRIPT", &script)
        .stderr(Stdio::inherit());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));

    // Positive exact inclusive boundary for created_from_ms / created_to_ms.
    let exact_early = rpc.call_raw(
        "session.search",
        json!({ "query": "boundary-needle", "filters": { "created_from_ms": t1, "created_to_ms": t1 } }),
        |_| json!(null),
    );
    let after_early = rpc.call_raw(
        "session.search",
        json!({ "query": "boundary-needle", "filters": { "created_from_ms": t1 + 1 } }),
        |_| json!(null),
    );
    // Positive exact inclusive boundary for event_filters from_ms / to_ms.
    let item_exact = rpc.call_raw(
        "session.search_items",
        json!({
            "session_id": "sess-early",
            "query": "boundary-needle",
            "event_filters": { "from_ms": t1 + 1000, "to_ms": t1 + 1000 }
        }),
        |_| json!(null),
    );
    let item_miss = rpc.call_raw(
        "session.search_items",
        json!({
            "session_id": "sess-early",
            "query": "boundary-needle",
            "event_filters": { "from_ms": t1 + 1001 }
        }),
        |_| json!(null),
    );
    let item_reversed = rpc.call_raw(
        "session.search_items",
        json!({
            "session_id": "sess-early",
            "query": "boundary-needle",
            "event_filters": { "from_ms": t2, "to_ms": t1 }
        }),
        |_| json!(null),
    );
    rpc.shutdown_and_wait();

    let early_ids: Vec<&str> = exact_early["result"]["data"]
        .as_array()
        .unwrap_or_else(|| panic!("expected data array: {exact_early}"))
        .iter()
        .filter_map(|r| r["session_id"].as_str())
        .collect();
    assert_eq!(early_ids, vec!["sess-early"], "created_from_ms/created_to_ms exact match: {exact_early}");

    let late_ids: Vec<&str> = after_early["result"]["data"]
        .as_array()
        .unwrap_or_else(|| panic!("expected data array: {after_early}"))
        .iter()
        .filter_map(|r| r["session_id"].as_str())
        .collect();
    assert_eq!(late_ids, vec!["sess-late"], "created_from_ms excludes t1 when bound is t1+1: {after_early}");

    assert_eq!(
        item_exact["result"]["data"].as_array().map(Vec::len),
        Some(1),
        "from_ms/to_ms inclusive boundary must match item at t1+1000: {item_exact}"
    );
    assert_eq!(
        item_miss["result"]["data"].as_array().map(Vec::len),
        Some(0),
        "from_ms=t1+1001 must exclude item at t1+1000: {item_miss}"
    );
    assert_eq!(item_reversed["error"]["code"], json!(-32602), "reversed from_ms/to_ms must fail: {item_reversed}");
    assert_eq!(
        item_reversed["error"]["data"]["code"],
        json!("invalid_arguments"),
        "reversed from_ms/to_ms error code: {item_reversed}"
    );
}
