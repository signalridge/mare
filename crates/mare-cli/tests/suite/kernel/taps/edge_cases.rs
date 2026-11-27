#[cfg(unix)]
use super::subscribe_replay::{
    peer_raw_call, read_raw_until_agent_end, read_raw_until_idle, rpc_prompt_wait, spawn_peer_rpc_fixture,
    wait_for_peer_raw,
};

/// Acceptance:
/// filtered `events.subscribe` replay skips excluded types without emitting spurious `lagged` /
/// `dropped` markers, keeps global `seq` numbers intact, and applies a subsequent filter-only
/// `events.subscribe` cleanly.
#[cfg(unix)]
#[test]
fn filtered_replay_preserves_seq_gaps_without_spurious_loss_and_updates_filter() {
    use serde_json::json;
    use std::io::BufRead;

    let (_home, mut rpc, session_id, sock) = spawn_peer_rpc_fixture(&["one", "two", "three"]);
    let (mut live_r, mut live_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut live_r, &mut live_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    assert!(
        peer_raw_call(&mut live_r, &mut live_w, 2, "events.subscribe", json!({ "session_id": session_id }))
            .0
            .get("error")
            .is_none()
    );

    // Turn 1: capture end seq + epoch cleanly while connected, then disconnect before turn 2.
    assert!(rpc_prompt_wait(&mut rpc, &session_id, "one").get("error").is_none());
    let live_t1 = read_raw_until_agent_end(&mut live_r);
    let last_t1 = &live_t1.last().unwrap().1["params"];
    let epoch = last_t1["session_epoch"].as_str().unwrap().to_string();
    let seq_t1 = last_t1["seq"].as_u64().unwrap();

    // Turn 2 runs while second peer is offline; primary `live_r` captures all unfiltered events.
    assert!(rpc_prompt_wait(&mut rpc, &session_id, "two").get("error").is_none());
    let live_t2 = read_raw_until_agent_end(&mut live_r);
    let expected_filtered: Vec<(String, u64)> = live_t2
        .iter()
        .filter(|(_, v)| matches!(v["params"]["type"].as_str(), Some("turn/started" | "turn/completed")))
        .map(|(_, v)| (v["params"]["type"].as_str().unwrap().to_string(), v["params"]["seq"].as_u64().unwrap()))
        .collect();
    assert_eq!(expected_filtered.len(), 2);
    assert!(expected_filtered[1].1 > expected_filtered[0].1 + 1, "expected intermediate events between start and end");

    // Reconnect and replay Turn 2 with `include: ["turn/started", "turn/completed"]`.
    let (mut p_r, mut p_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut p_r, &mut p_w, 1, "initialize", json!({"protocol_version": "3.0"})).0.get("error").is_none()
    );
    let (sub_reply, pre) = peer_raw_call(
        &mut p_r,
        &mut p_w,
        2,
        "events.subscribe",
        json!({
            "session_id": session_id,
            "session_epoch": epoch,
            "last_seq": seq_t1,
            "include": ["turn/started", "turn/completed"]
        }),
    );
    assert!(pre.is_empty(), "replay subscribe must not pre-deliver notifications: {pre:?}");
    assert_eq!(sub_reply["result"]["mode"], "replay", "{sub_reply}");
    assert_eq!(sub_reply["result"]["seq"].as_u64(), Some(seq_t1 + 1), "{sub_reply}");

    let mut replayed = Vec::new();
    loop {
        let mut line = String::new();
        assert!(p_r.read_line(&mut line).unwrap() > 0);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        match v["method"].as_str() {
            Some("lagged" | "dropped") => panic!("filtered replay must not emit spurious loss markers: {v}"),
            Some("event") => {
                let ty = v["params"]["type"].as_str().unwrap().to_string();
                let seq = v["params"]["seq"].as_u64().unwrap();
                replayed.push((ty.clone(), seq));
                if ty == "turn/completed" {
                    break;
                }
            }
            _ => {}
        }
    }
    assert_eq!(replayed, expected_filtered, "filtered replay must preserve exact seq gaps from live stream");

    // Narrow filter via `events.subscribe` and run Turn 3 -> only `turn/completed` arrives.
    let (narrow_ok, _) = peer_raw_call(
        &mut p_r,
        &mut p_w,
        3,
        "events.subscribe",
        json!({ "session_id": session_id, "include": ["turn/completed"] }),
    );
    assert!(narrow_ok.get("error").is_none(), "{narrow_ok}");
    assert!(rpc_prompt_wait(&mut rpc, &session_id, "three").get("error").is_none());
    let t3 = read_raw_until_agent_end(&mut p_r);
    let t3_types: Vec<&str> = t3.iter().map(|(_, v)| v["params"]["type"].as_str().unwrap()).collect();
    assert_eq!(
        t3_types,
        vec!["turn/completed"],
        "narrowed live subscription must only deliver turn/completed: {t3_types:?}"
    );

    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();
}

/// Acceptance:
/// over real RPC/peer, overflowing the bounded 4096-entry log causes `events.subscribe` with an
/// evicted `last_seq` to fall back to `mode = "baseline"` + `lagged { from_seq, to_seq }`, and
/// emitting an oversized (> 1 MiB) event over RPC delivers `dropped { count: 1 }` without seq gap.
#[cfg(unix)]
#[test]
fn rpc_evicted_last_seq_returns_baseline_with_lagged_and_oversized_emits_dropped() {
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::io::BufRead;

    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    let plugins = cfg.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        cfg.join("init.lua"),
        r#"mare.setup { peer = { allow = { "events.subscribe", "baseline.read", "agent.prompt", "agent.wait" } } }"#,
    )
    .unwrap();
    std::fs::write(
        plugins.join("c3-burst-probe.lua"),
        r#"return {
            name = "c3-burst-probe",
            inject = { "bus" },
            apply = function(ctx)
                ctx:on("turn/started", function(ev)
                    if ev and ev.text == "burst-evict" then
                        for i = 1, 4200 do
                            ctx.bus:emit("host/notice", { level = "info", text = "n-" .. tostring(i) })
                        end
                    elseif ev and ev.text == "emit-oversized" then
                        ctx.bus:emit("host/notice", { level = "info", text = "ok-before" })
                        local nums = {}
                        for i = 1, 600000 do nums[i] = 1 end
                        ctx.bus:emit("host/notice", { level = "info", text = "irreducible", nums = nums })
                        ctx.bus:emit("host/notice", { level = "info", text = "ok-after" })
                    end
                end)
            end,
        }"#,
    )
    .unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([
            { "text": "one" },
            { "text": "bursted" },
            { "text": "oversized-done" },
            { "text": "post-baseline" }
        ])
        .to_string(),
    )
    .unwrap();

    let mut env = BTreeMap::new();
    env.insert("MARE_HOME".into(), home.path().display().to_string());
    env.insert("MARE_FAUX_SCRIPT".into(), script.display().to_string());
    let mut rpc = crate::common::rpc::RpcClient::spawn(&["--rpc", "--peer", "--model", "faux/echo"], &env, home.path());
    let init = rpc.call("initialize", json!({"protocol_version": "3.0"}));
    let session_id = init["session"]["session_id"].as_str().unwrap().to_string();
    // Prevent the 4200-entry kernel burst from overflowing the primary RPC stdout queue (`OUT_QUEUE = 4096`).
    assert!(
        rpc.call("events.subscribe", json!({ "session_id": session_id, "exclude": ["host/notice"] }))
            .get("error")
            .is_none()
    );
    let sock = home.path().join("state/peers").join(format!("{}.sock", rpc.pid()));

    // Peer 1 observes initial turn and records `seq_t1` + `epoch`, then disconnects.
    let (mut p1_r, mut p1_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut p1_r, &mut p1_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    assert!(
        peer_raw_call(&mut p1_r, &mut p1_w, 2, "events.subscribe", json!({ "session_id": session_id }))
            .0
            .get("error")
            .is_none()
    );
    assert!(rpc_prompt_wait(&mut rpc, &session_id, "one").get("error").is_none());
    let (epoch, seq_t1) = read_raw_until_idle(&mut p1_r);
    drop((p1_r, p1_w));

    // Exceed the 4096-entry EventLog cap so `seq_t1` is evicted.
    assert!(rpc_prompt_wait(&mut rpc, &session_id, "burst-evict").get("error").is_none());

    // Reconnecting with the evicted `last_seq = seq_t1` must fall back to `mode = "baseline"` and emit `lagged`.
    let (mut p2_r, mut p2_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut p2_r, &mut p2_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let (sub_evicted, pre) = peer_raw_call(
        &mut p2_r,
        &mut p2_w,
        2,
        "events.subscribe",
        json!({ "session_id": session_id, "session_epoch": epoch, "last_seq": seq_t1 }),
    );
    assert!(pre.is_empty(), "baseline subscribe must not pre-deliver notifications: {pre:?}");
    assert_eq!(sub_evicted["result"]["mode"], "baseline", "{sub_evicted}");
    assert_eq!(sub_evicted["result"]["session_epoch"], epoch, "{sub_evicted}");

    let mut line = String::new();
    assert!(p2_r.read_line(&mut line).unwrap() > 0);
    let snap: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(snap["method"], "snapshot", "expected baseline snapshot after eviction: {snap}");
    assert!(snap["params"]["baseline_id"].as_str().is_some(), "missing baseline_id: {snap}");
    assert_eq!(snap["params"]["session_id"], session_id, "{snap}");
    assert_eq!(snap["params"]["session_epoch"], epoch, "{snap}");
    assert_eq!(
        snap["params"]["seq"].as_u64().map(|s| s + 1),
        sub_evicted["result"]["seq"].as_u64(),
        "snapshot seq + 1 must equal subscribe reply seq: sub={sub_evicted}, snap={snap}"
    );

    // Oversized >1MiB event emits `dropped { count: 1 }` on live RPC peer stream with zero seq gap.
    assert!(rpc_prompt_wait(&mut rpc, &session_id, "emit-oversized").get("error").is_none());
    let mut seq_before = None;
    let mut seq_after = None;
    let mut dropped_count = 0u64;
    loop {
        line.clear();
        assert!(p2_r.read_line(&mut line).unwrap() > 0);
        let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
        match msg["method"].as_str() {
            Some("dropped") => {
                dropped_count += msg["params"]["count"].as_u64().unwrap_or(0);
            }
            Some("event") => {
                if msg["params"]["type"] == "host/notice" {
                    let txt = msg["params"]["data"]["text"].as_str().unwrap_or("");
                    let seq = msg["params"]["seq"].as_u64().unwrap();
                    if txt == "ok-before" {
                        seq_before = Some(seq);
                    } else if txt == "ok-after" {
                        seq_after = Some(seq);
                    }
                } else if msg["params"]["type"] == "turn/completed" {
                    break;
                }
            }
            _ => {}
        }
    }
    assert_eq!(dropped_count, 1, "oversized (> 1 MiB) event must emit dropped count=1 over RPC");
    assert_eq!(
        seq_after,
        seq_before.map(|s| s + 1),
        "refused oversized event must not advance seq (before={seq_before:?}, after={seq_after:?})"
    );

    // Subsequent prompt keeps delivering cleanly after baseline+lagged+dropped.
    assert!(rpc_prompt_wait(&mut rpc, &session_id, "post-baseline").get("error").is_none());
    let post = read_raw_until_agent_end(&mut p2_r);
    assert!(post.last().unwrap().1["params"]["seq"].as_u64().unwrap() > seq_after.unwrap());

    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();
}

/// Acceptance (Codex `thread_name_persistence.rs:25`):
/// across process restart + `session.resume`, `session_epoch` rotates, `seq` restarts at `1`, and
/// calling `events.subscribe` with the previous process's `session_epoch` degrades to `mode = "baseline"` + `lagged`.
#[cfg(unix)]
#[test]
fn session_epoch_resets_seq_on_process_restart_and_rejects_old_epoch_replay() {
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::io::BufRead;

    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("init.lua"),
        r#"mare.setup { peer = { allow = { "events.subscribe", "baseline.read", "agent.prompt", "agent.wait" } } }"#,
    )
    .unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "proc1" }, { "text": "proc2" }]).to_string()).unwrap();

    let mut env = BTreeMap::new();
    env.insert("MARE_HOME".into(), home.path().display().to_string());
    env.insert("MARE_FAUX_SCRIPT".into(), script.display().to_string());

    // Process 1: run one turn in the initial persistent session, record (sid, epoch_1, tail_seq_1).
    let mut rpc1 = crate::common::rpc::RpcClient::spawn(&["--rpc", "--model", "faux/echo"], &env, home.path());
    let init1 = rpc1.call("initialize", json!({"protocol_version": "3.0"}));
    let sid = init1["session"]["session_id"].as_str().unwrap().to_string();
    rpc1.events.clear();
    assert!(rpc1.call("agent.prompt", json!({ "session_id": sid, "input": "proc1" })).get("error").is_none());
    while !rpc1.events.iter().any(|e| e["type"] == "turn/completed") {
        let m = rpc1.read();
        rpc1.dispatch(m);
    }
    let epoch_1 = rpc1.events.last().unwrap()["session_epoch"].as_str().unwrap().to_string();
    let seq_1 = rpc1.events.last().unwrap()["seq"].as_u64().unwrap();
    assert!(seq_1 >= 2, "expected multiple events in process 1: {seq_1}");
    let _ = rpc1.call("shutdown", json!({}));
    rpc1.wait_exit();

    // Process 2: resume the saved session from disk -> new `session_epoch`, `seq` restarts at 1.
    let mut rpc2 =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--peer", "--model", "faux/echo"], &env, home.path());
    let _ = rpc2.call("initialize", json!({"protocol_version": "3.0"}));
    let res = rpc2.call("session.load", json!({ "session_id": sid }));
    assert_eq!(res["session_id"].as_str(), Some(sid.as_str()), "{res}");
    rpc2.events.clear();
    assert!(rpc2.call("agent.prompt", json!({ "session_id": sid, "input": "proc2" })).get("error").is_none());
    while !rpc2.events.iter().any(|e| e["type"] == "turn/completed") {
        let m = rpc2.read();
        rpc2.dispatch(m);
    }
    let first_ev_2 = rpc2.events.first().unwrap().clone();
    let epoch_2 = first_ev_2["session_epoch"].as_str().unwrap().to_string();
    assert_ne!(epoch_2, epoch_1, "reopened session across restart must allocate a new session_epoch");
    assert_eq!(first_ev_2["seq"].as_u64(), Some(1), "new process epoch must restart seq at 1: {first_ev_2}");

    // Subscribe on peer with stale `epoch_1` and `last_seq = 1`: must degrade to `baseline` + `lagged`.
    let sock2 = home.path().join("state/peers").join(format!("{}.sock", rpc2.pid()));
    let (mut pr, mut pw) = wait_for_peer_raw(&sock2);
    assert!(
        peer_raw_call(&mut pr, &mut pw, 1, "initialize", json!({"protocol_version": "3.0"})).0.get("error").is_none()
    );
    let (sub_stale, pre) = peer_raw_call(
        &mut pr,
        &mut pw,
        2,
        "events.subscribe",
        json!({ "session_id": sid, "session_epoch": epoch_1, "last_seq": 1 }),
    );
    assert!(pre.is_empty());
    assert_eq!(sub_stale["result"]["mode"], "baseline", "stale epoch from prior process must not replay: {sub_stale}");
    assert_eq!(sub_stale["result"]["session_epoch"], epoch_2, "{sub_stale}");

    let mut line = String::new();
    assert!(pr.read_line(&mut line).unwrap() > 0);
    let snap: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(snap["method"], "snapshot", "expected baseline snapshot on epoch mismatch: {snap}");
    assert!(snap["params"]["baseline_id"].as_str().is_some(), "missing baseline_id: {snap}");
    assert_eq!(snap["params"]["session_epoch"], epoch_2, "{snap}");

    let _ = rpc2.call("shutdown", json!({}));
    rpc2.wait_exit();
}

/// Real bug caught during acceptance (`EventLogState` resets `epoch_counter = 1` on every process
/// start without a boot nonce): `initialize -> session.new` in process 1 produces `<sid>#3`, and
/// `initialize -> session.resume` in process 2 ALSO produces `<sid>#3`, causing silent epoch collision.
#[cfg(unix)]
#[test]
fn cross_process_restart_equal_call_sequence_epoch_collision_bug() {
    use serde_json::json;
    use std::collections::BTreeMap;

    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "proc1" }, { "text": "proc2" }]).to_string()).unwrap();

    let mut env = BTreeMap::new();
    env.insert("MARE_HOME".into(), home.path().display().to_string());
    env.insert("MARE_FAUX_SCRIPT".into(), script.display().to_string());

    let mut rpc1 = crate::common::rpc::RpcClient::spawn(&["--rpc", "--model", "faux/echo"], &env, home.path());
    let _ = rpc1.call("initialize", json!({"protocol_version": "3.0"}));
    let s1 = rpc1.call("session.create", json!({}));
    let sid = s1["session_id"].as_str().unwrap().to_string();
    rpc1.events.clear();
    assert!(rpc_prompt_wait(&mut rpc1, &sid, "proc1").get("error").is_none());
    while !rpc1.events.iter().any(|e| e["type"] == "turn/completed") {
        let m = rpc1.read();
        rpc1.dispatch(m);
    }
    let epoch_1 = rpc1.events.last().unwrap()["session_epoch"].as_str().unwrap().to_string();
    let _ = rpc1.call("shutdown", json!({}));
    rpc1.wait_exit();

    let mut rpc2 = crate::common::rpc::RpcClient::spawn(&["--rpc", "--model", "faux/echo"], &env, home.path());
    let _ = rpc2.call("initialize", json!({"protocol_version": "3.0"}));
    let _ = rpc2.call("session.load", json!({ "session_id": sid }));
    rpc2.events.clear();
    assert!(rpc_prompt_wait(&mut rpc2, &sid, "proc2").get("error").is_none());
    while !rpc2.events.iter().any(|e| e["type"] == "turn/completed") {
        let m = rpc2.read();
        rpc2.dispatch(m);
    }
    let epoch_2 = rpc2.events.first().unwrap()["session_epoch"].as_str().unwrap().to_string();
    let _ = rpc2.call("shutdown", json!({}));
    rpc2.wait_exit();

    assert_ne!(epoch_2, epoch_1, "different OS processes must not collide on session_epoch");
}

/// Acceptance:
/// deleted/cross-session `events.subscribe` rejects with `unknown_session`, external callers
/// have no `events.emit` RPC method, and caller-supplied envelope fields (`seq`, `session_epoch`)
/// never override host-assigned values.
#[cfg(unix)]
#[test]
fn out_of_process_boundary_rejects_cross_session_and_forged_envelope_fields() {
    use serde_json::json;

    let (_home, mut rpc, s1_id, sock) = spawn_peer_rpc_fixture(&["one", "two", "three"]);
    let (mut pr, mut pw) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut pr, &mut pw, 1, "initialize", json!({"protocol_version": "3.0"})).0.get("error").is_none()
    );

    // Persist `s1_id` to disk before switching away and deleting it.
    assert!(rpc_prompt_wait(&mut rpc, &s1_id, "one").get("error").is_none());

    // Create & delete a second session, then verify subscribing to deleted/foreign session fails.
    let s2 = rpc.call("session.create", json!({}));
    let s2_id = s2["session_id"].as_str().unwrap().to_string();
    assert_ne!(s2_id, s1_id);
    assert_eq!(rpc.call("session.close", json!({ "session_id": s1_id })), json!({}));
    let del = rpc.call("session.delete", json!({ "session_id": s1_id }));
    assert_eq!(del, json!({}), "{del}");

    let (del_err, _) =
        peer_raw_call(&mut pr, &mut pw, 2, "events.subscribe", json!({ "session_id": s1_id, "last_seq": 0 }));
    assert_eq!(del_err["error"]["data"]["code"], "unknown_session", "deleted session subscribe must fail: {del_err}");

    // Protocol exposes no `events.emit` method to external callers.
    let (emit_err, _) =
        peer_raw_call(&mut pr, &mut pw, 3, "events.emit", json!({ "type": "turn/completed", "seq": 99999 }));
    assert_eq!(emit_err["error"]["code"], -32601, "external events.emit must not exist: {emit_err}");

    // Subscribe to active `s2_id` with forged envelope field `seq` is rejected (`-32602` invalid_arguments);
    // canonical `last_seq = 99999` ignores forged seq and assigns real host `seq=1..`.
    let (forged_err, _) =
        peer_raw_call(&mut pr, &mut pw, 4, "events.subscribe", json!({ "session_id": s2_id, "seq": 99999 }));
    assert_eq!(forged_err["error"]["code"], -32602, "forged `seq` must be rejected: {forged_err}");
    let (sub_ok, _) =
        peer_raw_call(&mut pr, &mut pw, 5, "events.subscribe", json!({ "session_id": s2_id, "last_seq": 99999 }));
    assert!(sub_ok.get("error").is_none(), "{sub_ok}");

    rpc.events.clear();
    assert!(rpc_prompt_wait(&mut rpc, &s2_id, "two").get("error").is_none());
    let peer_ev = read_raw_until_agent_end(&mut pr);
    assert!(peer_ev.iter().all(|(_, v)| v["params"]["session_id"] == s2_id));
    assert!(
        peer_ev.iter().all(|(_, v)| v["params"]["seq"].as_u64().unwrap() < 99999),
        "forged seq in subscribe params must never affect host seq assignment: {peer_ev:?}"
    );

    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();
}
