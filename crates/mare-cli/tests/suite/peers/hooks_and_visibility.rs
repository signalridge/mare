use super::*;

/// A peer's `include` narrows its stream and never widens it. A peer
/// allowed `events.subscribe` that names a non-public event (`context/transform` carries the
/// whole history) receives only the public ones it named; `hooks.subscribe` refuses a
/// non-public event for a peer and accepts a public one.
#[test]
fn a_peer_include_or_hook_never_reaches_non_public_events() {
    let mut s = session(json!([{ "text": "primary done" }]), "allow = { 'events.subscribe', 'hooks.subscribe' }");
    let _wd = watchdog(s.pid(), 45);
    let mut peer = s.peer_with(json!({ "callbacks": true }));
    let sid = s.sid();
    let sub = peer.call_raw(
        "events.subscribe",
        json!({ "session_id": sid, "include": ["context/transform", "turn/started", "turn/completed"] }),
        |_| json!(null),
    );
    assert!(sub["error"].is_null(), "{sub}");
    let refused =
        peer.call_raw("hooks.subscribe", json!({ "event": "request/build", "callbacks": ["handle"] }), |_| json!(null));
    assert_eq!(refused["error"]["data"]["code"], "denied", "{refused}");
    let refused_pub =
        peer.call_raw("hooks.subscribe", json!({ "event": "status/changed", "callbacks": ["handle"] }), |_| {
            json!(null)
        });
    assert_eq!(refused_pub["error"]["data"]["code"], "denied", "hooks.subscribe must deny peers: {refused_pub}");

    let started = s.primary.call_raw("agent.prompt", json!({ "session_id": sid, "input": "go" }), |_| json!(null));
    assert!(started["error"].is_null(), "{started}");
    let id = s.primary.request("agent.wait", json!({ "session_id": sid, "turn_id": started["result"]["turn_id"] }));
    let mut seen = Vec::new();
    let mut barrier = None;
    loop {
        let m = peer.read();
        if m["method"] == "event" {
            let kind = m["params"]["type"].as_str().unwrap().to_string();
            if kind == "turn/completed" && barrier.is_none() {
                // every reply waits for the events queued before it: nothing is still in flight
                barrier = Some(peer.request("status.get", json!({})));
            }
            seen.push(kind);
        } else if barrier.is_some() && m.get("id") == barrier.map(|b| json!(b)).as_ref() {
            break;
        }
        assert_ne!(m["method"], "stream.failed", "{m}");
    }
    let result = s.primary.wait_raw(id, |_| json!(null));
    assert_eq!(result["result"]["text"], "primary done", "{result}");
    assert!(seen.contains(&"turn/started".to_string()) && seen.contains(&"turn/completed".to_string()), "{seen:?}");
    assert!(!seen.iter().any(|k| k == "context/transform"), "a non-public event reached the peer: {seen:?}");
    drop(peer);
    s.shutdown();
}

/// Decision 6: `hooks.subscribe` is `primary`-only and never grantable to peers even when listed
/// alongside `*` or with `public_events`.
#[test]
fn a_withdrawn_grant_stops_an_already_registered_peer_hook() {
    let wf_plugin = r#"return {
    name = "wfprobe",
    inject = { "bus" },
    contracts = {
        wfprobe = {
            methods = {},
            events = {
                pass = {
                    doc = "public waterfall probe",
                    mode = "waterfall",
                    expose = "both",
                    delivery = "required",
                    ignorable = false,
                    data = { type = "object", additionalProperties = true },
                    result = { type = "object", additionalProperties = true },
                    example = {},
                },
            },
        },
    },
    apply = function(ctx)
        ctx:on("request/build", function(req, ev)
            ctx.bus:waterfall("wfprobe/pass", { ok = true })
            return ev.next(req)
        end)
    end,
}"#;
    let turns = json!([{ "text": "1" }]);
    let s = session_env(turns, r#"allow = { "*", "hooks.subscribe" }"#, false, &[], &[("wfprobe.lua", wf_plugin)]);
    let _wd = watchdog(s.pid(), 45);
    let mut peer = s.peer_with(json!({ "callbacks": true }));
    let sub =
        peer.call_raw("hooks.subscribe", json!({ "event": "wfprobe/pass", "callbacks": ["handle"] }), |_| json!(null));
    assert_eq!(sub["error"]["data"]["code"], "denied", "hooks.subscribe must deny peers: {sub}");
    drop(peer);
    s.shutdown();
}

/// Decision 6: a peer cannot register a remote voter on `gate/check` even when listed in `allow`.
#[test]
fn a_granted_peer_registers_a_remote_voter_and_a_withdrawn_grant_stops_it() {
    let gate_plugin = r#"return {
    name = "gate",
    inject = { "policy" },
    contracts = {
        gate = {
            methods = {},
            events = {
                check = {
                    doc = "public policy check",
                    mode = "policy",
                    expose = "both",
                    delivery = "required",
                    ignorable = false,
                    data = { type = "object", properties = { tool = { type = "string" } } },
                    example = { tool = "read" },
                },
            },
        },
    },
    apply = function(ctx)
        ctx.policy:voter("tool/before", function(call)
            return ctx.policy:decide("gate/check", { { tool = call.name } })
        end)
    end,
}"#;
    let turns = json!([{ "text": "1" }]);
    let s = session_env(turns, r#"allow = { "*", "hooks.subscribe" }"#, false, &[], &[("gate.lua", gate_plugin)]);
    let _wd = watchdog(s.pid(), 45);
    let mut peer = s.peer_with(json!({ "callbacks": true }));
    let sub =
        peer.call_raw("hooks.subscribe", json!({ "event": "gate/check", "callbacks": ["handle"] }), |_| json!(null));
    assert_eq!(sub["error"]["data"]["code"], "denied", "hooks.subscribe must deny peers: {sub}");
    drop(peer);
    s.shutdown();
}

/// Decision 6: a peer cannot subscribe `hooks.subscribe` even on `status/changed` (use
/// `events.subscribe` for read-only event streaming instead).
#[test]
fn a_withdrawn_grant_stops_an_already_registered_peer_notification() {
    let turns = json!([{ "text": "1" }]);
    let s = session(turns, r#"allow = { "*", "hooks.subscribe" }"#);
    let _wd = watchdog(s.pid(), 45);
    let mut peer = s.peer_with(json!({ "callbacks": true }));
    let sub = peer
        .call_raw("hooks.subscribe", json!({ "event": "status/changed", "callbacks": ["handle"] }), |_| json!(null));
    assert_eq!(sub["error"]["data"]["code"], "denied", "hooks.subscribe must deny peers: {sub}");
    drop(peer);
    s.shutdown();
}

/// Process-only events (`expose = "process"`) never leave the process, whatever the
/// owner's `public_events` says: with `pipeline/*` and `provider/*`
/// configured public, a plugin's pipeline/approval-decided (a full in-process value) and
/// provider/request (a secret) reach neither the primary's RPC event stream nor a peer's, and a
/// remote hook on them is refused for both; a public marker emitted after them arrives.
#[test]
fn process_only_events_never_reach_rpc_streams_or_remote_hooks() {
    let leaker = r#"return { name = "leaker", inject = { "bus" }, apply = function(ctx)
  ctx:on("turn/started", function()
    pcall(ctx.bus.emit, ctx.bus, "pipeline/approval-decided", { secret = "synthetic-secret-1" })
    pcall(ctx.bus.emit, ctx.bus, "provider/request", { headers = { authorization = "synthetic-secret-2" } })
    ctx.bus:emit("host/notice", { level = "info", text = "after the process-only events" })
  end)
end }"#;
    let mut s = session_env(
        json!([{ "text": "done" }]),
        r#"allow = { "events.subscribe", "hooks.subscribe" }"#,
        false,
        &[],
        &[("leaker.lua", leaker)],
    );
    let _wd = watchdog(s.pid(), 60);
    let mut peer = s.peer_with(json!({ "callbacks": true }));
    let sid = s.sid();
    let sub = peer.call_raw(
        "events.subscribe",
        json!({ "session_id": sid, "include": ["pipeline/approval-decided", "provider/request", "host/notice"] }),
        |_| json!(null),
    );
    assert!(sub["error"].is_null(), "{sub}");
    for event in ["pipeline/approval-decided", "provider/request"] {
        let hooks = json!({ "session_id": sid, "event": event, "callbacks": ["handle"] });
        let r = s.primary.call_raw("hooks.subscribe", hooks.clone(), |_| json!(null));
        assert_eq!(err_code(&r), -32602, "primary {event}: {r}");
        let r = peer.call_raw("hooks.subscribe", hooks, |_| json!(null));
        assert_eq!(r["error"]["data"]["code"], "denied", "peer {event}: {r}");
    }
    let started = s.primary.call_raw("agent.prompt", json!({ "session_id": sid, "input": "go" }), |_| json!(null));
    assert!(started["error"].is_null(), "{started}");
    let id = s.primary.request("agent.wait", json!({ "session_id": sid, "turn_id": started["result"]["turn_id"] }));
    let mut primary_frames = Vec::new();
    loop {
        let m = s.primary.read();
        if m.get("id") == Some(&json!(id)) {
            break;
        }
        primary_frames.push(m);
    }
    let mut peer_frames = Vec::new();
    loop {
        let m = peer.read();
        let marker =
            m["params"]["type"] == "host/notice" && m["params"]["data"]["text"] == "after the process-only events";
        peer_frames.push(m);
        if marker {
            break;
        }
    }
    for (who, frames) in [("primary", &primary_frames), ("peer", &peer_frames)] {
        let text = serde_json::to_string(frames).unwrap();
        assert!(!text.contains("synthetic-secret"), "{who} got a process-only value: {text}");
        assert!(!text.contains("pipeline/") && !text.contains("provider/request"), "{who}: {text}");
    }
    assert!(
        serde_json::to_string(&primary_frames).unwrap().contains("after the process-only events"),
        "the public marker reached the primary"
    );
    s.shutdown();
}
