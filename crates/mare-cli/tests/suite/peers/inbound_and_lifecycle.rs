use super::*;

#[test]
fn invalid_messages_are_refused() {
    let s = session(json!([]), "");
    let mut c = s.peer();
    assert_eq!(err_code(&c.call_raw("peer.message", json!({ "text": "  " }), |_| json!(null))), -32602);
    // a `from.pid` that isn't the connecting process (the socket's credentials say otherwise)
    let forged = json!({ "from": { "name": "x", "pid": std::process::id() + 1 }, "text": "hi" });
    assert_eq!(err_code(&c.call_raw("peer.message", forged, |_| json!(null))), -32602);
    // input is bounded: a line over 1 MiB is answered with -32600 and the connection stays usable
    c.send(json!({ "jsonrpc": "2.0", "id": 99, "method": "peer.info", "params": { "pad": "x".repeat(1_100_000) } }));
    let r = c.read();
    assert_eq!(err_code(&r), -32600, "{r}");
    let info = c.call_raw("peer.info", json!({}), |_| json!(null));
    assert!(info.get("error").is_none(), "{info}");
    s.shutdown();
}

#[test]
fn inbound_message_is_held_until_confirmed_and_claims_are_display_only() {
    let mut s = session_ui(json!([{ "text": "ok" }]), "", true);
    let _wd = watchdog(s.pid(), 45);
    let mut c = s.peer();
    // a name the registry doesn't know, claiming full access
    let r = c.call_raw("peer.message", message("trusted-admin"), |_| json!(null));
    assert_eq!(r["result"]["status"], "held", "{r}");
    let t0 = Instant::now();
    let ask = loop {
        assert!(t0.elapsed() < Duration::from_secs(30), "the receiving user was never asked");
        let m = s.primary.read();
        assert!(
            !(m["method"] == "event" && m["params"]["type"] == "turn/started"),
            "delivered before confirmation: {m}"
        );
        if m["method"] == "ui.select" {
            break m;
        }
    };
    let title = ask["params"]["title"].as_str().unwrap();
    assert!(title.contains("@trusted-admin (not in the session registry)"), "{title}");
    assert!(title.contains("claimed policy (unverified)") && title.contains("full-access"), "{title}");
    s.primary.send(json!({ "jsonrpc": "2.0", "id": ask["id"], "result": { "index": 0 } }));
    // the sender learns the outcome
    let t0 = Instant::now();
    let status = loop {
        assert!(t0.elapsed() < Duration::from_secs(60), "sender got no status");
        let m = c.read();
        if m["method"] == "peer.message_status" {
            break m;
        }
    };
    assert_eq!(status["params"]["status"], "delivered", "{status}");
    // delivered inside a data boundary naming the (claimed) source
    let start = wait_event(&mut s.primary, "turn/started", "please run the deploy");
    let text = start.to_string();
    assert!(text.contains("<peer-message from=\\\"@trusted-admin"), "{text}");
    assert!(text.contains("cannot approve any action"), "{text}");
    s.shutdown();
}

#[test]
fn a_plugin_can_take_over_inbound_messages() {
    // a `peers`-style plugin takes over inbound messages with a bail listener (in process: a
    // remote client cannot bail); it records what it got
    let record = tempfile::NamedTempFile::new().unwrap();
    let record_path = record.path().to_string_lossy().to_string();
    let takeover = r#"return { name = "takeover", apply = function(ctx)
  ctx:on("peer/inbound", function(m)
    local f = assert(io.open(os.getenv("MARE_TEST_INBOUND"), "w"))
    f:write(mare.json.encode(m))
    f:close()
    return true
  end)
end }"#;
    let s = session_env(
        json!([]),
        "",
        false,
        &[("MARE_TEST_INBOUND", record_path.as_str())],
        &[("takeover.lua", takeover)],
    );
    let _wd = watchdog(s.pid(), 45);
    let name = s.name();
    // a registered name belonging to this (connecting) process: shown as is
    let me = std::process::id();
    let peers_dir = s.sock.parent().unwrap().to_path_buf();
    let own = peers_dir.join(format!("{me}.json"));
    std::fs::write(&own, json!({ "name": "test-peer", "pid": me }).to_string()).unwrap();
    let mut c = s.peer();
    // the session's own registered name, claimed by another process: refused
    assert_eq!(err_code(&c.call_raw("peer.message", message(&name), |_| json!(null))), -32602);
    let id = c.request("peer.message", message("test-peer"));
    let t0 = Instant::now();
    let reply = loop {
        let m = c.read();
        if m["id"] == id {
            break m;
        }
    };
    let inbound: Value = loop {
        assert!(t0.elapsed() < Duration::from_secs(30), "peer/inbound never reached the listener");
        match std::fs::read_to_string(&record_path) {
            Ok(text) if !text.is_empty() => break serde_json::from_str(&text).unwrap(),
            // sleep-ok: poll interval for the record file the peer process writes (bounded by the 30 s assert)
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    assert_eq!(inbound["from"]["name"], "test-peer", "{inbound}");
    assert_eq!(inbound["from"]["pid"], json!(me), "the credential's pid: {inbound}");
    assert!(inbound["message_id"].as_str().is_some_and(|s| !s.is_empty()), "{inbound}");
    assert!(inbound.get("id").is_none(), "{inbound}");
    assert_eq!(inbound["policy_verified"], false);
    assert_eq!(reply["result"]["status"], "handled", "{reply}");
    let _ = std::fs::remove_file(own);
    s.shutdown();
}

/// The peer registration goes after every app/shutdown listener: a plugin's app/shutdown listener
/// still finds the session's registry entry (so a final peer notification can go out), and once the
/// session has exited the entry is gone.
#[test]
fn the_peer_registration_outlives_the_app_shutdown_listeners() {
    let marker = tempfile::NamedTempFile::new().unwrap();
    let marker_path = marker.path().to_string_lossy().to_string();
    let probe = r#"return { name = "shutdown-probe", apply = function(ctx)
  ctx:on("app/shutdown", function()
    local registered = 0
    for _, entry in ipairs(mare.fs.list(mare.paths.state .. "/peers") or {}) do
      if entry.name:match("%.json$") then registered = registered + 1 end
    end
    local f = assert(io.open(mare.env("SHUTDOWN_MARKER"), "w"))
    f:write(tostring(registered))
    f:close()
  end)
end }"#;
    let s = session_env(
        json!([]),
        "",
        false,
        &[("SHUTDOWN_MARKER", marker_path.as_str())],
        &[("shutdown-probe.lua", probe)],
    );
    let _wd = watchdog(s.pid(), 60);
    let json_path = s.json_path.clone();
    assert!(json_path.exists(), "registered while running");
    s.shutdown();
    let seen = std::fs::read_to_string(&marker_path).unwrap();
    assert_eq!(seen, "1", "the app/shutdown listener ran after the peer registration was withdrawn");
    assert!(!json_path.exists(), "the registration outlived the session");
}

/// cell calls belong to the connection that makes them; `cells.{start,wait,cancel,reset}` are
/// `primary`-only even when listed in `peer.allow`, while `cells.list` (`peer`) isolates callers.
#[test]
fn a_peers_sandbox_reset_leaves_the_primarys_cells() {
    let mut s =
        session(json!([]), r#"allow = { "cells.start", "cells.reset", "cells.list", "cells.wait", "cells.cancel" }"#);
    let _wd = watchdog(s.pid(), 60);
    let st = s.primary.call_raw("status.get", json!({}), |_| json!(null));
    let sid = st["result"]["session"]["session_id"].as_str().expect("session_id").to_owned();
    let started = s.primary.call_raw(
        "cells.start",
        json!({ "session_id": sid, "cell_id": "cell-a", "code": "yield_control(); sleep(20000); return 'done'", "yield_time_ms": 1000 }),
        |_| json!(null),
    );
    assert_eq!(started["result"]["status"], "running", "{started}");
    let mut b = s.peer_with(json!({}));
    for (method, params) in [
        ("cells.start", json!({ "session_id": sid, "code": "return 1" })),
        ("cells.wait", json!({ "session_id": sid, "cell_id": "cell-a", "yield_time_ms": 200 })),
        ("cells.cancel", json!({ "session_id": sid, "cell_id": "cell-a" })),
        ("cells.reset", json!({ "session_id": sid })),
    ] {
        let resp = b.call_raw(method, params, |_| json!(null));
        assert_eq!(resp["error"]["data"]["code"], "denied", "{method} must be primary-only: {resp}");
    }
    let peer_listed = b.call_raw("cells.list", json!({ "session_id": sid }), |_| json!(null));
    assert_eq!(peer_listed["result"]["data"], json!([]), "peer must not see primary's cells: {peer_listed}");
    let listed = s.primary.call_raw("cells.list", json!({ "session_id": sid }), |_| json!(null));
    let cells = listed["result"]["data"].as_array().cloned().unwrap_or_default();
    assert!(
        cells.iter().any(|c| c["cell_id"] == "cell-a" && c["status"] == "running"),
        "the primary's cell is still running: {listed}"
    );
    s.primary.call_raw("cells.cancel", json!({ "session_id": sid, "cell_id": "cell-a" }), |_| json!(null));
    drop(b);
    s.shutdown();
}

#[test]
fn a_child_process_sends_to_its_own_session_and_the_message_is_held() {
    let mut s = session_ui(json!([{ "text": "ok" }]), "", true);
    let _wd = watchdog(s.pid(), 45);
    let session_id = s.primary.call_raw("session.list", json!({ "loaded": true }), |_| json!(null))["result"]["data"]
        [0]["session_id"]
        .as_str()
        .map(str::to_string);
    // what a tool's child process does: MARE_PEER_SOCKET (+ MARE_SESSION_ID) from its environment
    let mut send_cmd = mare_cmd();
    send_cmd
        .args(["peer", "send", "build finished: 3 warnings"])
        .env("MARE_PEER_SOCKET", &s.sock)
        .env("MARE_SESSION_ID", session_id.clone().unwrap_or_default())
        .env("MARE_HOME", s._home.path())
        .stdout(Stdio::piped());
    let send = ChildGroup::spawn(&mut send_cmd).unwrap();
    // the receiving user is asked first; the source names this session's child
    let t0 = Instant::now();
    let ask = loop {
        assert!(t0.elapsed() < Duration::from_secs(30), "the receiving user was never asked");
        let m = s.primary.read();
        if m["method"] == "ui.select" {
            break m;
        }
    };
    let title = ask["params"]["title"].as_str().unwrap().to_string();
    if session_id.is_some() {
        assert!(title.contains("a child process of this session"), "{title}");
    }
    s.primary.send(json!({ "jsonrpc": "2.0", "id": ask["id"], "result": { "index": 0 } }));
    let out = wait_output_timeout(send, Duration::from_secs(30));
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "held");
    // without the variable (and no --to) it refuses instead of guessing a session
    let mut cmd = mare_cmd();
    cmd.args(["peer", "send", "hello"]).env_remove("MARE_PEER_SOCKET").env("MARE_HOME", s._home.path());
    let out = run_output(&mut cmd);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("MARE_PEER_SOCKET is not set"));
    s.shutdown();
}

/// What a plugin publishes (`mare.peers.publish`) reaches the registry file other processes read,
/// and follows it: the `session` plugin's open session across `/name` and `/new`.
#[test]
fn published_fields_reach_the_registry_and_stay_current() {
    let mut s = session(json!([]), "");
    let _wd = watchdog(s.pid(), 50);
    let first = s.sid();
    registry_until(&s, "the session id", |e| e["plugins"]["session"]["session_id"] == first.as_str());

    s.primary.call_raw("commands.run", json!({ "line": "/name alpha" }), |_| json!(null));
    registry_until(&s, "the new name", |e| e["plugins"]["session"]["name"] == "alpha");

    s.primary.call_raw("commands.run", json!({ "line": "/new" }), |_| json!(null));
    let e = registry_until(&s, "a new session", |e| {
        e["plugins"]["session"]["session_id"].is_string() && e["plugins"]["session"]["session_id"] != first.as_str()
    });
    assert!(e["plugins"]["session"].get("name").is_none(), "the new session is unnamed: {e}");
    s.shutdown();
}

#[test]
fn disconnecting_peers_does_not_abort_primary_turn() {
    let mut s = session(json!([{ "text": "finished normally", "delay_ms": 30 }]), "");
    let _wd = watchdog(s.pid(), 45);
    let sid = s.sid();
    let a = s.peer();
    let b = s.peer();
    let started = s.primary.call_raw("agent.prompt", json!({"session_id": sid, "input": "go"}), |_| json!(null));
    assert!(started.get("error").is_none(), "{started}");
    let id = s.primary.request("agent.wait", json!({"session_id": sid, "turn_id": started["result"]["turn_id"]}));
    wait_event(&mut s.primary, "turn/started", "go");
    drop(a);
    drop(b);
    let result = s.primary.wait_raw(id, |_| json!(null));
    assert_eq!(result["result"]["text"], "finished normally", "{result}");
    s.shutdown();
}

#[test]
fn primary_output_loss_aborts_a_peer_started_turn() {
    let mut s =
        session(json!([{ "text": "x".repeat(4000), "delay_ms": 40 }]), "allow = { 'agent.prompt', 'agent.wait' }");
    let _wd = watchdog(s.pid(), 45);
    let sid = s.sid();
    s.primary
        .call_raw("events.subscribe", json!({"scope":"all","include":["turn/started","item/delta"]}), |_| json!(null));
    let mut peer = s.peer();
    let started =
        peer.call_raw("agent.prompt", json!({"session_id": sid, "input":"peer owns this turn"}), |_| json!(null));
    assert!(started.get("error").is_none(), "{started}");
    let id = peer.request("agent.wait", json!({"session_id": sid, "turn_id": started["result"]["turn_id"]}));
    wait_event(&mut s.primary, "turn/started", "peer owns this turn");
    let Session { mut primary, _home, .. } = s;
    primary.close_reader(); // close stdout while stdin stays open to isolate output-loss handling
    let result = peer.wait_raw(id, |_| json!(null));
    assert_eq!(result["result"]["error"]["code"], "aborted", "{result}");
    drop(peer);
    let _ = primary.wait_exit_status();
}

#[test]
fn peer_event_encoding_failure_does_not_abort_the_primary_turn() {
    // A public event (one a peer may receive) whose payload cannot be encoded.
    let unencodable = r#"
        return {
            name = "unencodable",
            inject = { "bus" },
            apply = function(ctx)
                ctx:on("turn/started", function()
                    ctx.bus:emit("host/notice", { level = "info", text = "x", bad = function() end })
                end)
            end,
        }
    "#;
    let mut s = session_env(
        json!([{ "text": "primary completed" }]),
        "allow = { 'events.subscribe' }",
        false,
        &[],
        &[("unencodable.lua", unencodable)],
    );
    let _wd = watchdog(s.pid(), 45);
    let sid = s.sid();
    // the primary does not select the broken event; only the peer's stream meets it
    s.primary.call_raw("events.subscribe", json!({ "scope": "all", "include": ["turn/completed"] }), |_| json!(null));
    let mut peer = s.peer();
    peer.call_raw("events.subscribe", json!({"session_id": sid, "include":["*"]}), |_| json!(null));
    let started = s.primary.call_raw("agent.prompt", json!({"session_id": sid, "input":"go"}), |_| json!(null));
    assert!(started.get("error").is_none(), "{started}");
    let id = s.primary.request("agent.wait", json!({"session_id": sid, "turn_id": started["result"]["turn_id"]}));
    let mut dropped_count = 0u64;
    while dropped_count == 0 {
        let line = peer.read();
        assert_ne!(line["method"], "stream.failed", "unencodable event must not fail peer stream: {line}");
        if line["method"] == "dropped" {
            dropped_count += line["params"]["count"].as_u64().unwrap_or(0);
        }
    }
    let result = s.primary.wait_raw(id, |_| json!(null));
    assert_eq!(result["result"]["text"], "primary completed", "{result}");
    let status = peer.call_raw("status.get", json!({}), |_| json!(null));
    assert!(status.get("result").is_some(), "peer connection must remain alive after dropped event: {status}");
    drop(peer);
    s.shutdown();
}

#[cfg(unix)]
#[test]
fn a_peer_that_stops_reading_is_disconnected_without_stopping_primary() {
    use std::io::{Read, Write};
    let mut s = session(json!([{ "text": "primary still works" }]), "");
    let _wd = watchdog(s.pid(), 45);
    let mut slow = std::os::unix::net::UnixStream::connect(&s.sock).unwrap();
    slow.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    slow.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"status.get\",\"params\":{}}\n";
    // Do not consume responses: enough requests to saturate the socket and bounded output.
    for _ in 0..100_000 {
        if slow.write_all(request).is_err() {
            break;
        }
    }
    let primary = s.prompt_wait("go");
    assert_eq!(primary["result"]["text"], "primary still works", "{primary}");
    // Once the peer is shut down, its already-written prefix may be drained, followed by EOF.
    let mut data = [0u8; 8192];
    loop {
        match slow.read(&mut data) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe) => break,
            Err(e) => panic!("stalled peer was not disconnected: {e}"),
        }
    }
    s.shutdown();
}

#[test]
fn peer_requires_successful_initialize_before_business_methods() {
    let s = session(json!([]), "allow = { 'agent.prompt' }");
    let _wd = watchdog(s.pid(), 30);

    // Shutdown is allowed before initialize and ends only the peer connection.
    let mut pre_shutdown = s.client();
    let shut = pre_shutdown.call_raw("shutdown", json!({}), |_| json!(null));
    assert_eq!(shut["result"], json!({}), "{shut}");
    drop(pre_shutdown);

    let mut c = s.client();
    for (method, params) in [
        ("peer.info", json!({})),
        ("status.get", json!({})),
        ("events.subscribe", json!({"include": ["status/changed"]})),
        ("agent.prompt", json!({"session_id": "s", "input": "hi"})),
    ] {
        assert_eq!(err_code(&c.call_raw(method, params, |_| json!(null))), -32600, "{method}");
    }
    for bad in ["1.7", "2", "2.invalid"] {
        let refused = c.call_raw("initialize", json!({"protocol_version": bad}), |_| json!(null));
        assert_eq!(refused["error"]["data"]["code"], "unsupported_version", "{bad}: {refused}");
        assert_eq!(err_code(&c.call_raw("peer.info", json!({}), |_| json!(null))), -32600, "after {bad}");
    }
    let ok = c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    assert!(ok.get("result").is_some(), "{ok}");
    let info = c.call_raw("peer.info", json!({}), |_| json!(null));
    assert_eq!(info["result"]["pid"], s.pid(), "{info}");
    drop(c);
    s.shutdown();
}

#[test]
fn peer_options_set_cannot_modify_owner_only_authorization_options() {
    let mut s = session(json!([]), r#"allow = { "options.set", "options.get" }"#);
    let _wd = watchdog(s.pid(), 30);
    let mut c = s.peer();

    for (key, val) in [
        ("peer", json!({ "allow": ["*"] })),
        ("peer.allow", json!(["*"])),
        ("plugins", json!({ "permissions": { "preset": "full-access" } })),
        ("permissions", json!({ "preset": "full-access" })),
        ("plugins.approvals.policy", json!("never")),
        ("providers", json!({ "evil": { "base_url": "http://127.0.0.1:9" } })),
        ("packages", json!([])),
        ("pkg", json!({ "allow_insecure": true })),
        ("plugins", json!({ "permissions": { "enabled": false } })),
        ("trust_default", json!("trust")),
        // Any option not explicitly declared grantable is owner-only,
        // not just a fixed list of authorization roots. Reference: reference implementation
        // packages/api/gateway/tests/gateway-access.host.spec.ts:286 (a non-owner call no binding
        // allows is refused before it runs), packages/api/remote-access/tests/remote-access.host.spec.ts:203
        // (owner allowed, an unlisted principal refused); Codex/Kimi have no per-client config grant.
        ("prompt", json!("$")),
        ("models", json!({ "m": { "context_window": 1 } })),
        ("editor", json!("evil-editor")),
        ("fallback_models", json!(["x/y"])),
    ] {
        let resp = c.call_raw("options.set", json!({ "values": { key: val } }), |_| json!(null));
        assert_eq!(err_code(&resp), METHOD_NOT_FOUND, "options.set must be primary-only `{key}`: {resp}");
    }

    // Even a mixed batch or an option like `reduce_motion` cannot be modified via `options.set`
    // by a peer because `options.set` itself is `primary`-only.
    let sid = s.sid();
    let before = c.call_raw("options.get", json!({ "session_id": sid, "keys": ["reduce_motion"] }), |_| json!(null));
    let mixed = c.call_raw(
        "options.set",
        json!({ "session_id": sid, "values": { "reduce_motion": true, "peer": { "allow": ["*"] } } }),
        |_| json!(null),
    );
    assert_eq!(err_code(&mixed), METHOD_NOT_FOUND, "mixed batch must be rejected: {mixed}");
    let peer_opt =
        c.call_raw("options.set", json!({ "session_id": sid, "values": { "reduce_motion": true } }), |_| json!(null));
    assert_eq!(err_code(&peer_opt), METHOD_NOT_FOUND, "peer options.set must be rejected: {peer_opt}");
    let after = c.call_raw("options.get", json!({ "session_id": sid, "keys": ["reduce_motion"] }), |_| json!(null));
    assert_eq!(after["result"]["reduce_motion"], before["result"]["reduce_motion"], "no partial write");

    // `peer.allow` was not escalated, so `agent.prompt` is still denied.
    assert_eq!(
        c.call_raw("agent.prompt", json!({ "session_id": "s", "input": "hi" }), |_| json!(null))["error"]["data"]["code"],
        "denied"
    );

    // Primary connection can still change owner-only options.
    let primary_ok = s.primary.call_raw(
        "options.set",
        json!({ "session_id": sid, "values": { "plugins.approvals.policy": "ask" } }),
        |_| json!(null),
    );
    assert!(primary_ok.get("error").is_none(), "primary may set approval_policy: {primary_ok}");

    drop(c);
    s.shutdown();
}

#[test]
fn inbound_message_approved_during_compaction_is_rejected_not_delivered() {
    let mut s = session_ui(
        json!([
            { "text": "first ".repeat(8000) },
            { "text": "second ".repeat(8000) },
            { "text": "third ".repeat(8000) },
            { "text": "slow summary ".repeat(200), "delay_ms": 40 }
        ]),
        "",
        true,
    );
    let _wd = watchdog(s.pid(), 45);
    let mut c = s.peer();
    s.primary.call_raw(
        "events.subscribe",
        json!({ "scope": "all", "include": ["compact/started", "compact/completed", "peer/released", "peer/rejected"] }),
        |_| json!(null),
    );
    for msg in ["msg 1", "msg 2", "msg 3"] {
        s.prompt_wait(msg);
    }

    // Start compaction asynchronously and wait for compact/started.
    let session_id = s.sid();
    let compact_id = s.primary.request("compact.run", json!({ "session_id": session_id }));
    let t0 = Instant::now();
    loop {
        assert!(t0.elapsed() < Duration::from_secs(30), "compact/started never arrived");
        let m = s.primary.read();
        assert_ne!(m.get("id").and_then(|v| v.as_u64()), Some(compact_id), "compact finished early: {m}");
        if m["method"] == "event" && m["params"]["type"] == "compact/started" {
            break;
        }
    }

    // Peer sends a message while compaction is in progress; user approves delivery.
    let r = c.call_raw("peer.message", message("trusted-admin"), |_| json!(null));
    assert_eq!(r["result"]["status"], "held", "{r}");
    let t0 = Instant::now();
    let ask = loop {
        assert!(t0.elapsed() < Duration::from_secs(30), "the receiving user was never asked");
        let m = s.primary.read();
        if m["method"] == "ui.select" {
            break m;
        }
    };
    s.primary.send(json!({ "jsonrpc": "2.0", "id": ask["id"], "result": { "index": 0 } }));

    // Peer must receive status="rejected" (not "delivered") with the compaction rejection reason.
    let t0 = Instant::now();
    let status = loop {
        assert!(t0.elapsed() < Duration::from_secs(30), "sender got no status");
        let m = c.read();
        if m["method"] == "peer.message_status" {
            break m;
        }
    };
    assert_eq!(
        status["params"]["status"], "rejected",
        "approved peer message during compaction must not report delivered: {status}"
    );
    let reason = status["params"]["reason"].as_str().unwrap_or("");
    assert!(reason.contains("compaction in progress"), "expected compaction rejection reason, got: {status}");

    // Public events must match the final status: peer/rejected emitted, peer/released NOT emitted.
    let mut saw_rejected = false;
    let t0 = Instant::now();
    while !saw_rejected {
        assert!(t0.elapsed() < Duration::from_secs(10), "expected peer/rejected event");
        let m = s.primary.read();
        if m["method"] == "event" {
            let ty = m["params"]["type"].as_str().unwrap_or("");
            assert_ne!(ty, "peer/released", "rejected peer message must not emit peer/released: {m}");
            if ty == "peer/rejected" {
                saw_rejected = true;
            }
        }
    }

    // Clean up compaction.
    let sid = s.sid();
    s.primary.call_raw("agent.cancel", json!({ "session_id": sid, "mode": "halt" }), |_| json!(null));
    drop(c);
    s.shutdown();
}

/// Headless runs without `--peer` do not register a peer entry or bind a peer socket.
#[test]
fn headless_without_peer_flag_does_not_register_peer_socket() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "headless ok" }]).to_string()).unwrap();
    let peers_dir = home.path().join("state/peers");
    let mut cmd = mare_cmd();
    cmd.args(["-p", "--json", "--model", "faux/echo", "--no-session", "hello"])
        .current_dir(work.path())
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .stdin(Stdio::null());
    let out = run_output(&mut cmd);
    assert!(out.status.success(), "headless run failed: {}", String::from_utf8_lossy(&out.stderr));
    let entries: Vec<_> = std::fs::read_dir(&peers_dir).into_iter().flatten().flatten().collect();
    assert!(entries.is_empty(), "headless without --peer must not create peer registry/socket files: {entries:?}");
}
