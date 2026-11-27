#[cfg(unix)]
pub(super) fn wait_for_peer_raw(
    sock: &std::path::Path,
) -> (std::io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !sock.exists() && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    let stream = std::os::unix::net::UnixStream::connect(sock).unwrap();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let writer = stream.try_clone().unwrap();
    (std::io::BufReader::new(stream), writer)
}

#[cfg(unix)]
pub(super) fn peer_raw_call(
    reader: &mut std::io::BufReader<std::os::unix::net::UnixStream>,
    writer: &mut std::os::unix::net::UnixStream,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> (serde_json::Value, Vec<(String, serde_json::Value)>) {
    use std::io::{BufRead, Write};
    let req = serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    writeln!(writer, "{req}").unwrap();
    writer.flush().unwrap();
    let mut pre = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap();
        assert!(n > 0, "EOF waiting for {method} id={id}");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
            return (v, pre);
        }
        pre.push((line, v));
    }
}

#[cfg(unix)]
pub(super) fn read_raw_until_agent_end(
    reader: &mut std::io::BufReader<std::os::unix::net::UnixStream>,
) -> Vec<(String, serde_json::Value)> {
    use std::io::BufRead;
    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap();
        assert!(n > 0, "EOF reading events");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v.get("method").and_then(serde_json::Value::as_str) == Some("event") {
            let done = v["params"]["type"] == "turn/completed";
            out.push((line, v));
            if done {
                return out;
            }
        }
    }
}

#[cfg(unix)]
pub(super) fn read_raw_until_idle(reader: &mut std::io::BufReader<std::os::unix::net::UnixStream>) -> (String, u64) {
    use std::io::BufRead;
    let _ = read_raw_until_agent_end(reader);
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap();
        assert!(n > 0, "EOF waiting for idle status/changed");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v.get("method").and_then(serde_json::Value::as_str) == Some("event")
            && v["params"]["type"] == "status/changed"
            && v["params"]["data"]["state"] == "idle"
        {
            return (v["params"]["session_epoch"].as_str().unwrap().to_string(), v["params"]["seq"].as_u64().unwrap());
        }
    }
}

#[cfg(unix)]
pub(super) fn spawn_peer_rpc_fixture_with_allow(
    turns: &[&str],
    allow_lua: &str,
) -> (tempfile::TempDir, crate::common::rpc::RpcClient, String, std::path::PathBuf) {
    use serde_json::json;
    use std::collections::BTreeMap;

    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("init.lua"), format!("mare.setup {{ peer = {{ allow = {allow_lua} }} }}")).unwrap();
    let script_items: Vec<serde_json::Value> = turns.iter().map(|t| json!({ "text": t })).collect();
    let script = home.path().join("faux.json");
    std::fs::write(&script, serde_json::Value::Array(script_items).to_string()).unwrap();

    let mut env = BTreeMap::new();
    env.insert("MARE_HOME".into(), home.path().display().to_string());
    env.insert("MARE_FAUX_SCRIPT".into(), script.display().to_string());
    let mut rpc = crate::common::rpc::RpcClient::spawn(&["--rpc", "--peer", "--model", "faux/echo"], &env, home.path());
    let init = rpc.call("initialize", json!({"protocol_version": "3.0"}));
    let session_id = init["session"]["session_id"]
        .as_str()
        .or_else(|| init["sessionId"].as_str())
        .unwrap_or_else(|| panic!("missing session_id in initialize result: {init}"))
        .to_string();
    let sock = home.path().join("state/peers").join(format!("{}.sock", rpc.pid()));
    (home, rpc, session_id, sock)
}

#[cfg(unix)]
pub(super) fn spawn_peer_rpc_fixture(
    turns: &[&str],
) -> (tempfile::TempDir, crate::common::rpc::RpcClient, String, std::path::PathBuf) {
    spawn_peer_rpc_fixture_with_allow(
        turns,
        r#"{ "events*", "events.subscribe", "events.list", "baseline.read", "agent.prompt", "agent.wait", "session.create" }"#,
    )
}

pub(super) fn rpc_prompt_wait(rpc: &mut crate::common::rpc::RpcClient, sid: &str, input: &str) -> serde_json::Value {
    let started = rpc.call("agent.prompt", serde_json::json!({ "session_id": sid, "input": input }));
    if started.get("error").is_some() {
        return started;
    }
    rpc.call("agent.wait", serde_json::json!({ "session_id": sid, "turn_id": started["turn_id"] }))
}

#[cfg(unix)]
#[test]
fn subscribe_with_last_seq_replays_the_missed_events_byte_for_byte_then_goes_live() {
    use serde_json::json;

    let (_home, mut rpc, session_id, sock) = spawn_peer_rpc_fixture(&["one", "two", "three"]);
    let (mut live_r, mut live_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut live_r, &mut live_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let (r_live_sub, _) =
        peer_raw_call(&mut live_r, &mut live_w, 2, "events.subscribe", json!({ "session_id": session_id }));
    assert!(r_live_sub.get("error").is_none(), "{r_live_sub}");

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

    let r1 = rpc_prompt_wait(&mut rpc, &session_id, "one");
    assert!(r1.get("error").is_none(), "{r1}");
    let _live_t1 = read_raw_until_agent_end(&mut live_r);
    let p1_t1 = read_raw_until_agent_end(&mut p1_r);
    let last_t1 = &p1_t1.last().unwrap().1;
    let epoch = last_t1["params"]["session_epoch"].as_str().unwrap().to_string();
    let seq_n = last_t1["params"]["seq"].as_u64().unwrap();
    drop((p1_r, p1_w));

    // Turn 2 happens while disconnected; live client records exact raw wire JSON lines.
    let r2 = rpc_prompt_wait(&mut rpc, &session_id, "two");
    assert!(r2.get("error").is_none(), "{r2}");
    let live_t2 = read_raw_until_agent_end(&mut live_r);
    let expected_raw_t2: Vec<String> = live_t2.iter().map(|(raw, _)| raw.clone()).collect();
    assert_eq!(live_t2.first().unwrap().1["params"]["seq"].as_u64(), Some(seq_n + 1));

    // Reconnect and resume from `last_seq = seq_n`.
    let (mut p2_r, mut p2_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut p2_r, &mut p2_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let (sub_reply, pre) = peer_raw_call(
        &mut p2_r,
        &mut p2_w,
        2,
        "events.subscribe",
        json!({ "session_id": session_id, "session_epoch": epoch, "last_seq": seq_n }),
    );
    assert!(pre.is_empty(), "replay subscribe must not pre-deliver notifications: {pre:?}");
    assert_eq!(sub_reply["result"]["mode"], "replay", "{sub_reply}");
    assert_eq!(sub_reply["result"]["session_epoch"], epoch, "{sub_reply}");
    assert_eq!(sub_reply["result"]["seq"].as_u64(), Some(seq_n + 1), "{sub_reply}");

    let replayed_t2 = read_raw_until_agent_end(&mut p2_r);
    let actual_raw_t2: Vec<String> = replayed_t2.iter().map(|(raw, _)| raw.clone()).collect();
    assert_eq!(actual_raw_t2, expected_raw_t2, "replayed JSON lines must be byte-for-byte identical to live");

    // Turn 3 must continue live on the same replayed subscription.
    let r3 = rpc_prompt_wait(&mut rpc, &session_id, "three");
    assert!(r3.get("error").is_none(), "{r3}");
    let live_t3 = read_raw_until_agent_end(&mut live_r);
    let p2_t3 = read_raw_until_agent_end(&mut p2_r);
    assert_eq!(
        p2_t3.iter().map(|(raw, _)| raw.as_str()).collect::<Vec<_>>(),
        live_t3.iter().map(|(raw, _)| raw.as_str()).collect::<Vec<_>>(),
        "live events after replay must match primary live stream"
    );

    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();
}

#[cfg(unix)]
#[test]
fn last_seq_equal_to_the_tail_goes_live_without_replay() {
    use serde_json::json;

    let (_home, mut rpc, session_id, sock) = spawn_peer_rpc_fixture(&["one", "two"]);
    let (mut p1_r, mut p1_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut p1_r, &mut p1_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let _ = peer_raw_call(&mut p1_r, &mut p1_w, 2, "events.subscribe", json!({ "session_id": session_id }));
    assert!(rpc_prompt_wait(&mut rpc, &session_id, "one").get("error").is_none());
    let (epoch, tail_seq) = read_raw_until_idle(&mut p1_r);
    drop((p1_r, p1_w));

    // Re-subscribe cleanly at `tail_seq`: mode must be "live", zero replay, zero lagged.
    let (mut p2_r, mut p2_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut p2_r, &mut p2_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let (sub_reply, pre) = peer_raw_call(
        &mut p2_r,
        &mut p2_w,
        2,
        "events.subscribe",
        json!({ "session_id": session_id, "session_epoch": epoch, "last_seq": tail_seq }),
    );
    assert!(pre.is_empty(), "live subscribe must not pre-deliver notifications: {pre:?}");
    assert_eq!(sub_reply["result"]["mode"], "live", "{sub_reply}");
    assert_eq!(sub_reply["result"]["session_epoch"], epoch, "{sub_reply}");
    assert_eq!(sub_reply["result"]["seq"].as_u64(), Some(tail_seq + 1), "{sub_reply}");

    assert!(rpc_prompt_wait(&mut rpc, &session_id, "two").get("error").is_none());
    let t2 = read_raw_until_agent_end(&mut p2_r);
    assert_eq!(t2.first().unwrap().1["params"]["seq"].as_u64(), Some(tail_seq + 1));

    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();
}

#[cfg(unix)]
#[test]
fn last_seq_beyond_the_tail_or_another_epoch_needs_a_baseline() {
    use serde_json::json;
    use std::io::BufRead;

    let (_home, mut rpc, session_id, sock) = spawn_peer_rpc_fixture(&["one", "two", "three"]);
    let (mut p1_r, mut p1_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut p1_r, &mut p1_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let _ = peer_raw_call(&mut p1_r, &mut p1_w, 2, "events.subscribe", json!({ "session_id": session_id }));
    assert!(rpc_prompt_wait(&mut rpc, &session_id, "one").get("error").is_none());
    let (epoch, tail_seq) = read_raw_until_idle(&mut p1_r);
    drop((p1_r, p1_w));

    // Case 1: last_seq beyond the tail -> mode = "baseline" + `snapshot`, then live.
    let (mut p2_r, mut p2_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut p2_r, &mut p2_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let (sub_beyond, pre_beyond) = peer_raw_call(
        &mut p2_r,
        &mut p2_w,
        2,
        "events.subscribe",
        json!({ "session_id": session_id, "session_epoch": epoch, "last_seq": tail_seq + 50 }),
    );
    assert!(pre_beyond.is_empty(), "baseline snapshot must follow events.subscribe reply: {pre_beyond:?}");
    assert_eq!(sub_beyond["result"]["mode"], "baseline", "{sub_beyond}");
    assert_eq!(sub_beyond["result"]["session_epoch"], epoch, "{sub_beyond}");
    let mut line = String::new();
    p2_r.read_line(&mut line).unwrap();
    let snap1: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(snap1["method"], "snapshot", "{snap1}");
    assert_eq!(snap1["params"]["session_id"], session_id, "{snap1}");
    assert_eq!(snap1["params"]["session_epoch"], epoch, "{snap1}");
    assert_eq!(snap1["params"]["seq"], sub_beyond["result"]["seq"].as_u64().unwrap() - 1, "{snap1}");
    assert!(snap1["params"]["baseline_id"].as_str().is_some_and(|s| !s.is_empty()), "{snap1}");
    let mut parts: Vec<&str> =
        snap1["params"]["parts"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    parts.sort();
    assert_eq!(parts, vec!["active_items", "entries"], "{snap1}");

    assert!(rpc_prompt_wait(&mut rpc, &session_id, "two").get("error").is_none());
    let t2 = read_raw_until_agent_end(&mut p2_r);
    assert_eq!(t2.first().unwrap().1["params"]["seq"].as_u64(), sub_beyond["result"]["seq"].as_u64());
    drop((p2_r, p2_w));

    // Case 2: mismatched session_epoch -> mode = "baseline" + `snapshot`.
    let (mut p3_r, mut p3_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut p3_r, &mut p3_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let (sub_epoch, pre_epoch) = peer_raw_call(
        &mut p3_r,
        &mut p3_w,
        2,
        "events.subscribe",
        json!({ "session_id": session_id, "session_epoch": "other#999", "last_seq": 1 }),
    );
    assert!(pre_epoch.is_empty(), "baseline epoch mismatch must not pre-deliver: {pre_epoch:?}");
    assert_eq!(sub_epoch["result"]["mode"], "baseline", "{sub_epoch}");
    assert_eq!(sub_epoch["result"]["session_epoch"], epoch, "{sub_epoch}");
    line.clear();
    p3_r.read_line(&mut line).unwrap();
    let snap2: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(snap2["method"], "snapshot", "{snap2}");

    assert!(rpc_prompt_wait(&mut rpc, &session_id, "three").get("error").is_none());
    let _t3 = read_raw_until_agent_end(&mut p3_r);
    drop((p3_r, p3_w));

    // Case 3: missing `last_seq` on a non-empty log -> mode = "baseline" + `snapshot`.
    let (mut p4_r, mut p4_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut p4_r, &mut p4_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let (sub_missing, pre_missing) =
        peer_raw_call(&mut p4_r, &mut p4_w, 2, "events.subscribe", json!({ "session_id": session_id }));
    assert!(pre_missing.is_empty(), "baseline missing last_seq must not pre-deliver: {pre_missing:?}");
    assert_eq!(sub_missing["result"]["mode"], "baseline", "{sub_missing}");
    assert_eq!(sub_missing["result"]["session_epoch"], epoch, "{sub_missing}");
    line.clear();
    p4_r.read_line(&mut line).unwrap();
    let snap3: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(snap3["method"], "snapshot", "{snap3}");
    assert!(snap3["params"]["seq"].as_u64().unwrap() > tail_seq, "{snap3}");

    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();
}

#[test]
fn session_create_subscribes_by_default_and_subscribe_false_opts_out() {
    use serde_json::json;
    use std::collections::BTreeMap;

    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "s1" }, { "text": "s2" }, { "text": "s3" }]).to_string()).unwrap();

    let mut env = BTreeMap::new();
    env.insert("MARE_HOME".into(), home.path().display().to_string());
    env.insert("MARE_FAUX_SCRIPT".into(), script.display().to_string());
    let mut rpc = crate::common::rpc::RpcClient::spawn(&["--rpc", "--model", "faux/echo"], &env, home.path());
    let _ = rpc.call("initialize", json!({"protocol_version": "3.0"}));

    // 1. Default `session.create {}` subscribes the connection.
    let s1 = rpc.call("session.create", json!({}));
    let s1_id = s1["session_id"].as_str().unwrap().to_string();
    rpc.events.clear();
    let p1 = rpc_prompt_wait(&mut rpc, &s1_id, "go 1");
    assert!(p1.get("error").is_none(), "{p1}");
    while !rpc.events.iter().any(|e| e["type"] == "turn/completed") {
        let msg = rpc.read();
        rpc.dispatch(msg);
    }
    assert!(
        rpc.events.iter().any(|e| e["type"] == "turn/completed" && e["session_id"] == s1_id),
        "default session.create must subscribe caller: {:?}",
        rpc.events
    );

    // 2. `session.create { subscribe: false }` opts out of events for the new session.
    let s2 = rpc.call("session.create", json!({ "subscribe": false }));
    let s2_id = s2["session_id"].as_str().unwrap().to_string();
    assert_ne!(s2_id, s1_id);
    rpc.events.clear();
    let p2 = rpc_prompt_wait(&mut rpc, &s2_id, "go 2");
    assert!(p2.get("error").is_none(), "{p2}");
    // Drain any buffered notifications up to an `options.get` barrier.
    let _ = rpc.call("options.get", json!({ "session_id": s2_id }));
    assert!(
        !rpc.events.iter().any(|e| e["session_id"] == s2_id),
        "subscribe: false must opt out of new session events: {:?}",
        rpc.events
    );

    // 3. `session.load { session_id, subscribe: false }` and `session.fork { session_id, subscribe: false }` opt out
    // across actual prompts, whereas default `session.fork { session_id }` subscribes.
    let r_res = rpc.call("session.load", json!({ "session_id": s1_id, "subscribe": false }));
    assert_eq!(r_res["session_id"].as_str(), Some(s1_id.as_str()));
    let f_opt_out = rpc.call("session.fork", json!({ "session_id": s1_id, "subscribe": false }));
    let f_opt_id = f_opt_out["session_id"].as_str().unwrap().to_string();
    rpc.events.clear();
    let p_opt = rpc_prompt_wait(&mut rpc, &f_opt_id, "go 3");
    assert!(p_opt.get("error").is_none(), "{p_opt}");
    let _ = rpc.call("options.get", json!({ "session_id": f_opt_id }));
    assert!(
        !rpc.events.iter().any(|e| e["session_id"] == f_opt_id),
        "session.fork subscribe: false must opt out: {:?}",
        rpc.events
    );
    let r_res_again = rpc.call("session.load", json!({ "session_id": s1_id, "subscribe": false }));
    assert_eq!(r_res_again["session_id"].as_str(), Some(s1_id.as_str()));
    let p_res_opt = rpc_prompt_wait(&mut rpc, &s1_id, "go 3b");
    assert!(p_res_opt.get("error").is_none(), "{p_res_opt}");
    let _ = rpc.call("options.get", json!({ "session_id": s1_id }));
    // `s1` already had a subscription from step 1 (`session.create`); switching away & back with
    // `subscribe: false` still requires a prompt when we check a fresh connection without prior s1 sub.
    let f_res = rpc.call("session.fork", json!({ "session_id": s1_id }));
    let f_id = f_res["session_id"].as_str().unwrap().to_string();
    rpc.events.clear();
    let p3 = rpc_prompt_wait(&mut rpc, &f_id, "go 4");
    assert!(p3.get("error").is_none(), "{p3}");
    while !rpc.events.iter().any(|e| e["type"] == "turn/completed") {
        let msg = rpc.read();
        rpc.dispatch(msg);
    }
    assert!(
        rpc.events.iter().any(|e| e["type"] == "turn/completed" && e["session_id"] == f_id),
        "default session.fork must subscribe caller: {:?}",
        rpc.events
    );

    // Deleting a session cleans up its subscription slot; `session.load { subscribe: false }`
    // on a fresh connection opts out across an actual prompt.
    let _ = rpc.call("session.close", json!({ "session_id": s1_id }));
    let del_s1 = rpc.call("session.delete", json!({ "session_id": s1_id }));
    assert_eq!(del_s1, json!({}), "{del_s1}");
    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();

    std::fs::write(&script, json!([{ "text": "resumed opt out" }]).to_string()).unwrap();
    let mut rpc2 = crate::common::rpc::RpcClient::spawn(&["--rpc", "--model", "faux/echo"], &env, home.path());
    let _ = rpc2.call("initialize", json!({"protocol_version": "3.0"}));
    let res_opt = rpc2.call("session.load", json!({ "session_id": f_id, "subscribe": false }));
    assert_eq!(res_opt["session_id"].as_str(), Some(f_id.as_str()), "{res_opt}");
    rpc2.events.clear();
    let p_res2 = rpc_prompt_wait(&mut rpc2, &f_id, "resumed opt out");
    assert!(p_res2.get("error").is_none(), "{p_res2}");
    let _ = rpc2.call("options.get", json!({ "session_id": f_id }));
    assert!(
        !rpc2.events.iter().any(|e| e["session_id"] == f_id),
        "session.resume subscribe: false must opt out of resumed session events: {:?}",
        rpc2.events
    );
    let _ = rpc2.call("shutdown", json!({}));
    rpc2.wait_exit();
}

#[cfg(unix)]
#[test]
fn multi_session_subscription_and_replace_keep_sessions_isolated() {
    use serde_json::json;

    let (_home, mut rpc, s1_id, sock) = spawn_peer_rpc_fixture(&["one", "two", "three"]);
    let (mut pr, mut pw) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut pr, &mut pw, 1, "initialize", json!({"protocol_version": "3.0"})).0.get("error").is_none()
    );

    // Subscribe to s1, then run turn 1 in s1.
    let (sub_s1, _) = peer_raw_call(&mut pr, &mut pw, 2, "events.subscribe", json!({ "session_id": s1_id }));
    assert!(sub_s1.get("error").is_none(), "{sub_s1}");
    assert!(rpc_prompt_wait(&mut rpc, &s1_id, "one").get("error").is_none());
    let _t1 = read_raw_until_agent_end(&mut pr);

    // Replace s1 cursor with narrow include=["turn/completed"], switch host to s2, and subscribe to s2 too.
    let (sub_s1_narrow, _) = peer_raw_call(
        &mut pr,
        &mut pw,
        3,
        "events.subscribe",
        json!({ "session_id": s1_id, "include": ["turn/completed"] }),
    );
    assert!(sub_s1_narrow.get("error").is_none(), "{sub_s1_narrow}");

    let s2 = rpc.call("session.create", json!({}));
    let s2_id = s2["session_id"].as_str().unwrap().to_string();
    assert_ne!(s2_id, s1_id);
    let (sub_s2, _) = peer_raw_call(
        &mut pr,
        &mut pw,
        4,
        "events.subscribe",
        json!({ "session_id": s2_id, "include": ["turn/started", "turn/completed"] }),
    );
    assert!(sub_s2.get("error").is_none(), "{sub_s2}");

    assert!(rpc_prompt_wait(&mut rpc, &s2_id, "two").get("error").is_none());
    let t2 = read_raw_until_agent_end(&mut pr);
    let s2_types: Vec<&str> = t2.iter().map(|(_, v)| v["params"]["type"].as_str().unwrap()).collect();
    assert_eq!(
        s2_types,
        vec!["turn/started", "turn/completed"],
        "s2 must deliver exactly its cursor without duplicates"
    );
    assert!(t2.iter().all(|(_, v)| v["params"]["session_id"] == s2_id));

    // Switch back to s1 with `subscribe: false`: s1's own narrow `["turn/completed"]` cursor must remain intact
    // (unaffected by subscribing to s2) and emit only `turn/completed` for turn 3.
    let res_s1 = rpc.call("session.load", json!({ "session_id": s1_id, "subscribe": false }));
    assert!(res_s1.get("error").is_none(), "{res_s1}");
    assert!(rpc_prompt_wait(&mut rpc, &s1_id, "three").get("error").is_none());
    let t3 = read_raw_until_agent_end(&mut pr);
    let s1_types: Vec<&str> = t3.iter().map(|(_, v)| v["params"]["type"].as_str().unwrap()).collect();
    assert_eq!(s1_types, vec!["turn/completed"], "s1 cursor must keep its own filter independently of s2");
    assert!(t3.iter().all(|(_, v)| v["params"]["session_id"] == s1_id));

    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();
}

#[cfg(unix)]
#[test]
fn peer_cross_session_and_unauthorized_events_subscribe_are_refused() {
    use serde_json::json;
    use std::collections::BTreeMap;

    // 1. Authorized peer (`events.subscribe`) cannot subscribe to a switched-away or nonexistent session,
    //    and omitting `session_id` targets the currently open session rather than initialize's stale id.
    let (_home, mut rpc, s1_id, sock) = spawn_peer_rpc_fixture(&["one", "two"]);
    let (mut pr, mut pw) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut pr, &mut pw, 1, "initialize", json!({"protocol_version": "3.0"})).0.get("error").is_none()
    );

    assert!(rpc_prompt_wait(&mut rpc, &s1_id, "one").get("error").is_none());
    let s2 = rpc.call("session.create", json!({}));
    let s2_id = s2["session_id"].as_str().unwrap().to_string();
    assert_ne!(s2_id, s1_id);
    assert_eq!(rpc.call("session.close", json!({ "session_id": s1_id })), json!({}));

    let (cross_err, _) =
        peer_raw_call(&mut pr, &mut pw, 2, "events.subscribe", json!({ "session_id": s1_id, "last_seq": 0 }));
    assert_eq!(
        cross_err["error"]["data"]["code"], "unknown_session",
        "cross-session subscribe to closed s1 must be rejected: {cross_err}"
    );
    let (missing_err, _) =
        peer_raw_call(&mut pr, &mut pw, 3, "events.subscribe", json!({ "session_id": "nonexistent-session" }));
    assert_eq!(
        missing_err["error"]["data"]["code"], "unknown_session",
        "unknown session_id subscribe must be rejected: {missing_err}"
    );

    // Omitting `session_id` on a peer must fail with `invalid_arguments`; explicit `s2_id` succeeds.
    let (omitted_err, _) = peer_raw_call(&mut pr, &mut pw, 4, "events.subscribe", json!({}));
    assert_eq!(
        omitted_err["error"]["data"]["code"], "invalid_arguments",
        "omitting session_id on peer events.subscribe must fail: {omitted_err}"
    );
    let (explicit_ok, _) = peer_raw_call(&mut pr, &mut pw, 5, "events.subscribe", json!({ "session_id": s2_id }));
    assert!(explicit_ok.get("error").is_none(), "{explicit_ok}");
    assert!(rpc_prompt_wait(&mut rpc, &s2_id, "two").get("error").is_none());
    let t2 = read_raw_until_agent_end(&mut pr);
    assert!(t2.iter().all(|(_, v)| v["params"]["session_id"] == s2_id), "{t2:?}");
    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();

    // 2. Peer with `allow = { "events.subscribe" }` receives initial stream AND may call `events.subscribe`
    //    (bare `"events"` is no longer a valid grant).
    let (_home2, mut rpc2, s2_id, sock2) = spawn_peer_rpc_fixture_with_allow(&["passive"], r#"{ "events.subscribe" }"#);
    let (mut p2_r, mut p2_w) = wait_for_peer_raw(&sock2);
    assert!(
        peer_raw_call(&mut p2_r, &mut p2_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let (sub_ok, _) = peer_raw_call(&mut p2_r, &mut p2_w, 2, "events.subscribe", json!({ "session_id": s2_id }));
    assert!(sub_ok.get("error").is_none(), "allow = events must authorize events.subscribe: {sub_ok}");
    assert!(rpc_prompt_wait(&mut rpc2, &s2_id, "passive").get("error").is_none());
    let passive_ev = read_raw_until_agent_end(&mut p2_r);
    assert!(passive_ev.iter().any(|(_, v)| v["params"]["type"] == "turn/completed"), "{passive_ev:?}");
    let _ = rpc2.call("shutdown", json!({}));
    rpc2.wait_exit();

    // 3. Peer with no grants (`allow = {}`) cannot call `events.subscribe`.
    let (_home3, mut rpc3, s3_id, sock3) = spawn_peer_rpc_fixture_with_allow(&[], r#"{}"#);
    let (mut p3_r, mut p3_w) = wait_for_peer_raw(&sock3);
    assert!(
        peer_raw_call(&mut p3_r, &mut p3_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let (denied3, _) = peer_raw_call(&mut p3_r, &mut p3_w, 2, "events.subscribe", json!({ "session_id": s3_id }));
    assert!(denied3.get("error").is_some(), "ungranted peer must be rejected: {denied3}");
    let _ = rpc3.call("shutdown", json!({}));
    rpc3.wait_exit();

    // 4. First `scope = "all"` subscription on a connection without an existing cursor still updates
    //    `default_filter` for subsequent `session.create {}` switches (SF2).
    let home4 = tempfile::tempdir().unwrap();
    let script4 = home4.path().join("faux.json");
    std::fs::write(&script4, json!([{ "text": "after-filter" }]).to_string()).unwrap();
    let mut env4 = BTreeMap::new();
    env4.insert("MARE_HOME".into(), home4.path().display().to_string());
    env4.insert("MARE_FAUX_SCRIPT".into(), script4.display().to_string());
    let mut rpc4 = crate::common::rpc::RpcClient::spawn(&["--rpc", "--model", "faux/echo"], &env4, home4.path());
    let _ = rpc4.call("initialize", json!({"protocol_version": "3.0"}));
    let _ = rpc4.call("session.create", json!({ "subscribe": false }));
    let _ = rpc4.call("events.subscribe", json!({ "scope": "all", "include": ["turn/completed"] }));
    let s_next = rpc4.call("session.create", json!({}));
    let s_next_id = s_next["session_id"].as_str().unwrap().to_string();
    rpc4.events.clear();
    assert!(rpc_prompt_wait(&mut rpc4, &s_next_id, "after-filter").get("error").is_none());
    while !rpc4.events.iter().any(|e| e["type"] == "turn/completed") {
        let msg = rpc4.read();
        rpc4.dispatch(msg);
    }
    let next_types: Vec<&str> =
        rpc4.events.iter().filter(|e| e["session_id"] == s_next_id).filter_map(|e| e["type"].as_str()).collect();
    assert_eq!(next_types, vec!["turn/completed"], "initial filter_only must update default_filter: {next_types:?}");
    let _ = rpc4.call("shutdown", json!({}));
    rpc4.wait_exit();
}

#[cfg(unix)]
#[test]
fn rpc_narrow_include_then_widen_and_inflight_concurrent_replay() {
    use serde_json::json;
    use std::collections::BTreeMap;

    // 1. `--rpc` starts with narrow `include = ["agent/*"]`, runs turn 1 emitting private
    //    `myplugin/progress` (with zero Lua listeners), then widens via `events.subscribe`.
    //    `taps_changed()` must keep `bus.quiet` dynamic so turn 2 delivers `myplugin/progress`.
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    let plugins = cfg.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        cfg.join("init.lua"),
        r#"mare.setup { peer = { allow = { "events*", "events.subscribe", "agent.prompt", "agent.wait" } } }"#,
    )
    .unwrap();
    std::fs::write(
        plugins.join("progress-probe.lua"),
        r#"return {
            name = "progress-probe",
            inject = { "bus" },
            contracts = {
                myplugin = {
                    owner = "progress-probe",
                    events = {
                        progress = {
                            doc = "probe progress event",
                            mode = "emit",
                            expose = "both",
                            delivery = "required",
                            ignorable = false,
                            data = { type = "object", additionalProperties = true },
                            example = { step = 1 },
                        },
                    },
                },
            },
            apply = function(ctx)
                local count = 0
                ctx:on("turn/started", function()
                    count = count + 1
                    ctx.bus:emit("myplugin/progress", { step = count })
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
            { "text": "two" },
            { "tool_calls": [{ "name": "bash", "arguments": { "command": "touch should-not-run", "sandbox_permissions": "all", "justification": "test hold" } }] },
            { "text": "three" }
        ])
        .to_string(),
    )
    .unwrap();

    let mut env = BTreeMap::new();
    env.insert("MARE_HOME".into(), home.path().display().to_string());
    env.insert("MARE_FAUX_SCRIPT".into(), script.display().to_string());
    let mut rpc = crate::common::rpc::RpcClient::spawn(
        &["--rpc", "--peer", "--permission", "read-only", "--approval-policy", "ask", "--model", "faux/echo"],
        &env,
        home.path(),
    );
    let init = rpc.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "approvals": true } }));
    let s1_id = init["session"]["session_id"].as_str().unwrap().to_string();
    let sock = home.path().join("state/peers").join(format!("{}.sock", rpc.pid()));

    let (mut live_r, mut live_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut live_r, &mut live_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    assert!(
        peer_raw_call(&mut live_r, &mut live_w, 2, "events.subscribe", json!({ "session_id": s1_id }))
            .0
            .get("error")
            .is_none()
    );

    let narrow = rpc.call("events.subscribe", json!({ "session_id": s1_id, "include": ["turn/*"] }));
    assert!(narrow.get("error").is_none(), "{narrow}");
    assert!(rpc_prompt_wait(&mut rpc, &s1_id, "one").get("error").is_none());
    while !rpc.events.iter().any(|e| e["type"] == "turn/completed") {
        let msg = rpc.read();
        rpc.dispatch(msg);
    }
    assert!(
        !rpc.events.iter().any(|e| e["type"] == "myplugin/progress"),
        "narrow subscription must exclude myplugin/progress on turn 1"
    );

    // Widen filter via `events.subscribe`.
    rpc.events.clear();
    let widen = rpc
        .call("events.subscribe", json!({ "session_id": s1_id, "include": ["myplugin/progress", "turn/completed"] }));
    assert!(widen.get("error").is_none(), "{widen}");
    assert!(rpc_prompt_wait(&mut rpc, &s1_id, "two").get("error").is_none());
    while !rpc.events.iter().any(|e| e["type"] == "turn/completed") {
        let msg = rpc.read();
        rpc.dispatch(msg);
    }
    let widened_ev = rpc.events.iter().find(|e| e["type"] == "myplugin/progress").unwrap_or_else(|| {
        panic!(
            "M2 regression: widened subscription missed myplugin/progress due to stale bus.quiet cache: {:?}",
            rpc.events
        )
    });
    assert_eq!(widened_ev["data"]["step"], 2, "{widened_ev}");
    let epoch = widened_ev["session_epoch"].as_str().unwrap().to_string();

    // 2. Concurrent in-flight turn: pause mid-turn on `approval.request`, then subscribe a peer from
    //    `last_seq = 0`, verifying zero notifications precede the reply and seamless replay->live.
    rpc.events.clear();
    let p_hold = rpc.call("agent.prompt", json!({ "session_id": s1_id, "input": "hold" }));
    let prompt_req = rpc.request("agent.wait", json!({ "session_id": s1_id, "turn_id": p_hold["turn_id"] }));
    let approval_req = loop {
        let msg = rpc.read();
        if msg.get("id").is_some() && msg["method"] == "approval.request" {
            break msg;
        }
        rpc.dispatch(msg);
    };
    let (mut pr, mut pw) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut pr, &mut pw, 1, "initialize", json!({"protocol_version": "3.0"})).0.get("error").is_none()
    );
    let (sub_mid, pre_mid) = peer_raw_call(
        &mut pr,
        &mut pw,
        2,
        "events.subscribe",
        json!({ "session_id": s1_id, "session_epoch": epoch, "last_seq": 0 }),
    );
    assert!(pre_mid.is_empty(), "in-flight replay subscribe must not pre-deliver notifications: {pre_mid:?}");
    assert_eq!(sub_mid["result"]["mode"], "replay", "{sub_mid}");
    assert_eq!(sub_mid["result"]["seq"], 1, "{sub_mid}");

    // Read until peer receives the mid-turn `approval/requested` event, then release the approval.
    let mut mid_raw_all = Vec::new();
    loop {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut pr, &mut line).unwrap();
        let ev: serde_json::Value = serde_json::from_str(&line).unwrap();
        if ev["method"] != "event" {
            continue;
        }
        mid_raw_all.push(line);
        if ev["params"]["type"] == "approval/requested" {
            break;
        }
    }
    assert!(!mid_raw_all.is_empty());

    rpc.send(json!({
        "jsonrpc": "2.0",
        "id": approval_req["id"],
        "result": { "decision": "deny", "reason": "release hold" }
    }));
    loop {
        let msg = rpc.read();
        if msg.get("method").is_none() && msg["id"] == prompt_req {
            assert!(msg.get("error").is_none(), "{msg}");
            break;
        }
        rpc.dispatch(msg);
    }
    for (raw, _) in read_raw_until_agent_end(&mut pr) {
        mid_raw_all.push(raw);
    }
    // Live peer received turns 1, 2, and 3 (plus interleaved status/changed) up to turn 3's
    // `turn/completed`; mid-turn replay->live peer must match byte-for-byte.
    let mut live_raw_all = Vec::new();
    let mut agent_ends = 0;
    while agent_ends < 3 {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut live_r, &mut line).unwrap();
        let ev: serde_json::Value = serde_json::from_str(&line).unwrap();
        if ev["method"] != "event" {
            continue;
        }
        if ev["params"]["type"] == "turn/completed" {
            agent_ends += 1;
        }
        live_raw_all.push(line);
    }
    assert_eq!(mid_raw_all, live_raw_all, "mid-turn replay->live stream diverged from live peer");

    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();
}

#[cfg(unix)]
#[test]
fn a_reconnecting_client_rebuilds_the_same_prefix_as_a_live_client() {
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

    // Live client observes turn 1 & turn 2 (drain through idle so turn boundaries stay clean).
    assert!(rpc_prompt_wait(&mut rpc, &session_id, "one").get("error").is_none());
    let live_t1 = read_raw_until_idle(&mut live_r);
    assert!(rpc_prompt_wait(&mut rpc, &session_id, "two").get("error").is_none());
    let live_t2 = read_raw_until_idle(&mut live_r);

    // Reconnecting client subscribes without prior seq -> gets mode = "baseline" and `snapshot`
    // immediately after the subscribe reply.
    let (mut rc_r, mut rc_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut rc_r, &mut rc_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let (sub_res, pre) =
        peer_raw_call(&mut rc_r, &mut rc_w, 2, "events.subscribe", json!({ "session_id": session_id }));
    assert!(pre.is_empty(), "snapshot must not precede subscribe reply: {pre:?}");
    assert_eq!(sub_res["result"]["mode"], "baseline", "{sub_res}");
    let mut line = String::new();
    rc_r.read_line(&mut line).unwrap();
    let snap: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(snap["method"], "snapshot", "{snap}");
    let baseline_id = snap["params"]["baseline_id"].as_str().unwrap().to_string();
    let snap_seq = snap["params"]["seq"].as_u64().unwrap();
    assert_eq!(sub_res["result"]["seq"].as_u64(), Some(snap_seq + 1), "{sub_res} vs {snap}");

    // Trigger turn 3 (new writes and completion!) *while* the client has not read `entries` yet.
    assert!(rpc_prompt_wait(&mut rpc, &session_id, "three").get("error").is_none());
    let live_t3 = read_raw_until_agent_end(&mut live_r);

    // Page `entries` and `active_items` via `baseline.read`; collect post-S notifications returned
    // before the `baseline.read` replies!
    let mut buffered = Vec::new();
    let mut entries = Vec::new();
    let mut cursor: Option<String> = None;
    let mut rpc_id = 10u64;
    loop {
        let mut params = json!({ "baseline_id": baseline_id, "part": "entries" });
        if let Some(c) = cursor.take() {
            params["cursor"] = json!(c);
        }
        let (reply, pre) = peer_raw_call(&mut rc_r, &mut rc_w, rpc_id, "baseline.read", params);
        rpc_id += 1;
        buffered.extend(pre.into_iter().map(|(_, v)| v));
        assert!(reply.get("error").is_none(), "{reply}");
        assert!(reply["result"].get("status").is_none(), "baseline became stale unexpectedly: {reply}");
        entries.extend(reply["result"]["data"].as_array().unwrap().iter().cloned());
        match reply["result"].get("next_cursor").and_then(serde_json::Value::as_str) {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }
    let (active_reply, pre) = peer_raw_call(
        &mut rc_r,
        &mut rc_w,
        rpc_id,
        "baseline.read",
        json!({ "baseline_id": baseline_id, "part": "active_items" }),
    );
    buffered.extend(pre.into_iter().map(|(_, v)| v));
    assert!(active_reply.get("error").is_none(), "{active_reply}");

    // Cut `entries` must contain turn 1 and turn 2 (4 messages: user+assistant x2), NOT turn 3.
    let msg_texts: Vec<String> = entries
        .iter()
        .filter(|e| e["type"] == "message" && e["message"].get("context_disclosure").is_none())
        .filter_map(|e| e["message"]["content"].as_array().and_then(|c| c.first()).and_then(|b| b["text"].as_str()))
        .map(str::to_string)
        .collect();
    assert_eq!(msg_texts, vec!["one", "one", "two", "two"], "cut entries must match pre-turn-3 prefix: {entries:?}");

    // Drain post-S turn-3 events until `turn/completed` -> strictly contiguous starting at snap_seq + 1.
    while !buffered.iter().any(|v| v["method"] == "event" && v["params"]["type"] == "turn/completed") {
        let mut l = String::new();
        assert!(rc_r.read_line(&mut l).unwrap() > 0);
        buffered.push(serde_json::from_str(&l).unwrap());
    }
    let post_events: Vec<&serde_json::Value> = buffered.iter().filter(|v| v["method"] == "event").collect();
    assert_eq!(post_events.first().unwrap()["params"]["seq"].as_u64(), Some(snap_seq + 1));
    for w in post_events.windows(2) {
        let a = w[0]["params"]["seq"].as_u64().unwrap();
        let b = w[1]["params"]["seq"].as_u64().unwrap();
        assert_eq!(b, a + 1, "post-snapshot events must be contiguous");
    }
    let rc_t3_types: Vec<&str> =
        post_events.iter().map(|v| v["params"]["type"].as_str().unwrap()).filter(|t| *t != "status/changed").collect();
    let live_t3_types: Vec<&str> =
        live_t3.iter().map(|(_, v)| v["params"]["type"].as_str().unwrap()).filter(|t| *t != "status/changed").collect();
    assert_eq!(rc_t3_types, live_t3_types);
    let _ = (live_t1, live_t2);

    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();
}

#[cfg(unix)]
#[test]
fn baseline_is_stale_after_an_epoch_change_or_idle_expiry() {
    use serde_json::json;
    use std::io::BufRead;

    let (_home, mut rpc, session_id, sock) = spawn_peer_rpc_fixture(&["one", "two"]);
    let (mut p_r, mut p_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut p_r, &mut p_w, 1, "initialize", json!({"protocol_version": "3.0"})).0.get("error").is_none()
    );
    let (sub1, _) = peer_raw_call(&mut p_r, &mut p_w, 2, "events.subscribe", json!({ "session_id": session_id }));
    assert!(sub1.get("error").is_none(), "{sub1}");
    let _ = rpc_prompt_wait(&mut rpc, &session_id, "one");

    // New subscriber gets a baseline with baseline_id.
    let (mut c1_r, mut c1_w) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut c1_r, &mut c1_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let (sub_b1, _) = peer_raw_call(&mut c1_r, &mut c1_w, 2, "events.subscribe", json!({ "session_id": session_id }));
    assert_eq!(sub_b1["result"]["mode"], "baseline", "{sub_b1}");
    let mut line = String::new();
    c1_r.read_line(&mut line).unwrap();
    let snap1: serde_json::Value = serde_json::from_str(&line).unwrap();
    let bid1 = snap1["params"]["baseline_id"].as_str().unwrap().to_string();

    // Close and reload `session_id` (rotating its session_epoch) while keeping `s2` open -> existing baseline becomes stale!
    let s2 = rpc.call("session.create", json!({}));
    assert!(s2.get("error").is_none(), "{s2}");
    let s2_id = s2["session_id"].as_str().unwrap().to_string();
    assert!(rpc.call("session.close", json!({ "session_id": session_id })).get("error").is_none());
    assert!(rpc.call("session.load", json!({ "session_id": session_id })).get("error").is_none());
    let (stale_epoch, _) =
        peer_raw_call(&mut c1_r, &mut c1_w, 3, "baseline.read", json!({ "baseline_id": bid1, "part": "entries" }));
    assert_eq!(stale_epoch["error"]["data"]["code"], "baseline.stale", "{stale_epoch}");

    // Also: once all parts of a baseline are completely read, the baseline is released -> subsequent read is stale.
    let (sub_b2, _) = peer_raw_call(&mut c1_r, &mut c1_w, 4, "events.subscribe", json!({ "session_id": s2_id }));
    assert_eq!(sub_b2["result"]["mode"], "baseline", "{sub_b2}");
    let snap2 = loop {
        line.clear();
        c1_r.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v["method"] == "snapshot" {
            break v;
        }
    };
    let bid2 = snap2["params"]["baseline_id"].as_str().unwrap().to_string();
    for part in snap2["params"]["parts"].as_array().unwrap() {
        let p = part.as_str().unwrap();
        let mut cursor: Option<String> = None;
        let mut id = 10;
        loop {
            let mut req = json!({ "baseline_id": bid2, "part": p });
            if let Some(c) = cursor.take() {
                req["cursor"] = json!(c);
            }
            let (r, _) = peer_raw_call(&mut c1_r, &mut c1_w, id, "baseline.read", req);
            id += 1;
            assert!(r["result"].get("data").is_some(), "{r}");
            match r["result"].get("next_cursor").and_then(serde_json::Value::as_str) {
                Some(n) => cursor = Some(n.to_string()),
                None => break,
            }
        }
    }
    let (after_release, _) =
        peer_raw_call(&mut c1_r, &mut c1_w, 99, "baseline.read", json!({ "baseline_id": bid2, "part": "entries" }));
    assert_eq!(after_release["error"]["data"]["code"], "baseline.stale", "{after_release}");

    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();
}

#[cfg(unix)]
#[test]
fn baseline_read_is_denied_to_ungranted_peer_and_isolated_between_granted_peers() {
    use serde_json::json;
    use std::io::BufRead;

    let (_home, mut rpc, session_id, sock) =
        spawn_peer_rpc_fixture_with_allow(&["one"], r#"{ "events*", "events.subscribe" }"#);

    let (mut pr, mut pw) = wait_for_peer_raw(&sock);
    assert!(
        peer_raw_call(&mut pr, &mut pw, 1, "initialize", json!({"protocol_version": "3.0"})).0.get("error").is_none()
    );
    let (denied, _) =
        peer_raw_call(&mut pr, &mut pw, 2, "baseline.read", json!({ "baseline_id": "any", "part": "entries" }));
    assert!(denied.get("error").is_some(), "ungranted peer must be denied baseline.read: {denied}");

    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();

    // Two granted peers cannot read each other's `baseline_id`.
    let (_home2, mut rpc2, s2_id, sock2) = spawn_peer_rpc_fixture(&["one"]);
    let (mut p1_r, mut p1_w) = wait_for_peer_raw(&sock2);
    let (mut p2_r, mut p2_w) = wait_for_peer_raw(&sock2);
    assert!(
        peer_raw_call(&mut p1_r, &mut p1_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    assert!(
        peer_raw_call(&mut p2_r, &mut p2_w, 1, "initialize", json!({"protocol_version": "3.0"}))
            .0
            .get("error")
            .is_none()
    );
    let (sub1, _) = peer_raw_call(&mut p1_r, &mut p1_w, 2, "events.subscribe", json!({ "session_id": s2_id }));
    assert_eq!(sub1["result"]["mode"], "baseline", "{sub1}");
    let mut line = String::new();
    p1_r.read_line(&mut line).unwrap();
    let snap1: serde_json::Value = serde_json::from_str(&line).unwrap();
    let bid1 = snap1["params"]["baseline_id"].as_str().unwrap();

    let (stolen, _) =
        peer_raw_call(&mut p2_r, &mut p2_w, 2, "baseline.read", json!({ "baseline_id": bid1, "part": "entries" }));
    assert_eq!(
        stolen["error"]["data"]["code"], "baseline.stale",
        "other peer must see baseline.stale for foreign baseline_id: {stolen}"
    );
    let _ = session_id;
    let _ = rpc2.call("shutdown", json!({}));
    rpc2.wait_exit();
}
