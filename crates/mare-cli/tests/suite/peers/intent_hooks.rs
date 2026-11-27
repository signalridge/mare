//! fs/write-intent and fs/edit-intent reach a remote voter end to end: the primary's
//! `hooks.subscribe` voter is called on a real write/edit via `callback.call` and its deny refuses
//! the tool; a peer may not vote (the points are not public); a withdrawn `capabilities.callbacks`
//! stops the calls.
use super::*;

/// One prompt: answers each `callback.call` with a deny (recording its arguments) and collects the
/// `agent/tool-result` events until the prompt's reply.
fn turn_denied(s: &mut Session) -> (Vec<Value>, Vec<Value>) {
    let sid = s.sid();
    let started = s.primary.call_raw("agent.prompt", json!({ "session_id": sid, "input": "go" }), |_| json!(null));
    assert!(started["error"].is_null(), "{started}");
    let id = s.primary.request("agent.wait", json!({ "session_id": sid, "turn_id": started["result"]["turn_id"] }));
    let (mut calls, mut results) = (Vec::new(), Vec::new());
    loop {
        let m = s.primary.read();
        assert_ne!(m["method"], "stream.failed", "{m}");
        if m["method"] == "callback.call" {
            calls.push(m["params"]["arguments"].clone());
            let deny = json!({ "value": { "decision": "deny", "reason": "rpc voter says no" } });
            s.primary.send(json!({ "jsonrpc": "2.0", "id": m["id"], "result": deny }));
        } else if m["method"] == "event"
            && m["params"]["type"] == "item/completed"
            && m["params"]["data"]["item"]["type"] == "tool_call"
        {
            results.push(m["params"]["data"]["item"].clone());
        } else if m.get("method").is_some() && m.get("id").is_some() {
            s.primary.send(json!({ "jsonrpc": "2.0", "id": m["id"], "result": null }));
        } else if m["id"] == id {
            assert!(m["error"].is_null(), "{m}");
            return (calls, results);
        }
    }
}

#[test]
fn the_primary_votes_on_write_and_edit_intents_and_a_peer_may_not() {
    let write = json!({ "tool_calls": [{ "name": "write", "arguments": { "path": "new.txt", "content": "w\n" } }] });
    let script = json!([
        write, { "text": "done" },
        { "tool_calls": [{ "name": "read", "arguments": { "path": "e.txt" } }] },
        { "tool_calls": [{ "name": "edit", "arguments": { "path": "e.txt", "old_string": "a", "new_string": "b" } }] },
        { "text": "done" },
        write, { "text": "done" },
    ]);
    let mut s = session(script, "allow = { 'hooks.subscribe' }");
    let _wd = watchdog(s.pid(), 90);
    let work = s._home.path().join("work");
    std::fs::write(work.join("e.txt"), "a\n").unwrap();
    let mut peer = s.peer_with(json!({ "callbacks": true }));
    let sid = s.sid();
    for point in ["fs/write-intent", "fs/edit-intent"] {
        let hooks = json!({ "session_id": sid, "event": point, "callbacks": ["handle"] });
        let refused = peer.call_raw("hooks.subscribe", hooks.clone(), |_| json!(null));
        assert_eq!(refused["error"]["data"]["code"], "denied", "a peer voting on {point}: {refused}");
        let sub = s.primary.call_raw("hooks.subscribe", hooks, |_| json!(null));
        assert!(sub["error"].is_null(), "{point}: {sub}");
    }
    let sub = s
        .primary
        .call_raw("events.subscribe", json!({ "scope": "all", "include": ["item/completed"] }), |_| json!(null));
    assert!(sub["error"].is_null(), "{sub}");

    for (point, file, before) in [("fs/write-intent", "new.txt", None), ("fs/edit-intent", "e.txt", Some("a\n"))] {
        let (calls, results) = turn_denied(&mut s);
        let call = calls.iter().find(|c| c["event"] == point).unwrap_or_else(|| panic!("{point}: {calls:?}"));
        assert_eq!(call["mode"], "policy", "{call}");
        assert!(call["args"].to_string().contains(file), "{point}: the args name the path: {call}");
        let refused = results.last().unwrap_or_else(|| panic!("{point}: no tool result"));
        assert!(refused.to_string().contains("rpc voter says no"), "{point}: {refused}");
        assert_eq!(std::fs::read_to_string(work.join(file)).ok().as_deref(), before, "{point}: the file is unchanged");
    }

    let r = s.primary.call_raw(
        "connection.set_capabilities",
        json!({ "capabilities": { "callbacks": false, "ui": false, "approvals": false, "elicitation": false } }),
        |_| json!(null),
    );
    assert!(r["error"].is_null(), "{r}");
    let (calls, results) = turn_denied(&mut s);
    assert!(calls.is_empty(), "a revoked voter is still called: {calls:?}");
    eprintln!("after revocation: {results:?}");
    // after revocation the remote voter fails closed: the write is refused
    assert!(results.last().is_some_and(|r| r.to_string().contains("callback.call")), "{results:?}");
    assert!(!work.join("new.txt").exists(), "a write went through after revocation");
    drop(peer);
    s.shutdown();
}
