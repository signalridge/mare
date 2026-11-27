use super::*;

/// Seed two sessions whose ids share the prefix `abcd` and a third, `Secret Gamma`, to archive.
fn seed_ambiguous_sessions(s: &Session) {
    let work = dunce::canonicalize(s._home.path().join("work")).unwrap_or_else(|_| s._home.path().join("work"));
    let dir = s._home.path().join("state/sessions").join(mare_store::cwd_slug(&work));
    std::fs::create_dir_all(&dir).unwrap();
    for (n, id, name) in [
        (1, "abcd1111-0000-0000-0000-000000000001", "Secret Alpha"),
        (2, "abcd2222-0000-0000-0000-000000000002", "Secret Beta"),
        (3, "efef3333-0000-0000-0000-000000000003", "Secret Gamma"),
    ] {
        std::fs::write(
            dir.join(format!("2026-01-0{n}T00-00-00_{n}.jsonl")),
            format!(
                "{}\n{}\n",
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": id, "cwd": work }),
                json!({ "type": "name", "name": name }),
            ),
        )
        .unwrap();
    }
}

/// The whole error object a restricted caller received must not carry these.
const OTHER_SESSIONS: [&str; 6] = ["Secret Alpha", "Secret Beta", "abcd1111", "abcd2222", "efef3333", "2 sessions"];

fn assert_names_no_other_session(resp: &Value, ctx: &str) {
    let whole = resp["error"].to_string();
    for leak in OTHER_SESSIONS {
        assert!(!whole.contains(leak), "{ctx}: `{leak}` in {whole}");
    }
    // nothing beyond the code rides along, and the message carries no count or id digits (the
    // selectors these tests send contain none, so any digit came from other sessions)
    assert_eq!(resp["error"]["data"], json!({ "code": "invalid_arguments" }), "{ctx}: {whole}");
    let msg = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(!msg.chars().any(|c| c.is_ascii_digit()), "{ctx}: a number in {msg}");
}

/// b13a / b16 SELECTOR-LEAK: a selector error from `session.resume` is an argument error for
/// everyone, but only a caller that may list sessions hears which sessions matched (ambiguous
/// prefix: titles and short ids; archived match: its short id). A peer granted only
/// `session.resume` learns nothing about other sessions — checked on the whole serialized error
/// object, not just its message.
/// Reference test: Kimi packages/acp-server/test/e2e-turn.test.ts:635 and :659 (the serialized
/// error carries no internal secret).
#[test]
fn session_resume_selector_errors_name_other_sessions_only_to_callers_who_may_list() {
    for (grant, may_list) in
        [(r#"allow = { "session.load" }"#, false), (r#"allow = { "session.load", "session.list" }"#, true)]
    {
        let mut s = session(json!([]), grant);
        seed_ambiguous_sessions(&s);
        let archived = s.primary.call_raw(
            "session.archive",
            json!({ "session_id": "efef3333-0000-0000-0000-000000000003" }),
            |_| json!(null),
        );
        assert!(archived.get("error").is_none(), "{archived}");
        let mut c = s.peer();

        let ambiguous = c.call_raw("session.load", json!({ "session_id": "abcd" }), |_| json!(null));
        assert_eq!(ambiguous["error"]["data"]["code"], "invalid_arguments", "{ambiguous}");
        assert!(ambiguous["error"]["message"].as_str().unwrap().contains("give more of the id"), "{ambiguous}");
        let by_name = c.call_raw("session.load", json!({ "session_id": "Secret Gamma" }), |_| json!(null));
        assert_eq!(by_name["error"]["data"]["code"], "invalid_arguments", "{by_name}");
        assert!(by_name["error"]["message"].as_str().unwrap().contains("archived"), "{by_name}");
        if may_list {
            let msg = ambiguous["error"]["message"].as_str().unwrap();
            for detail in ["Secret Alpha", "Secret Beta", "abcd1111", "abcd2222", "2 sessions"] {
                assert!(msg.contains(detail), "granted: `{detail}` missing from {ambiguous}");
            }
            assert!(by_name["error"]["message"].as_str().unwrap().contains("efef3333"), "{by_name}");
        } else {
            assert_names_no_other_session(&ambiguous, "ambiguous, resume-only grant");
            assert_names_no_other_session(&by_name, "archived, resume-only grant");
        }
        let none = c.call_raw("session.load", json!({ "session_id": "zzzz-none" }), |_| json!(null));
        assert_eq!(none["error"]["data"]["code"], "invalid_arguments", "{none}");
        assert!(none["error"]["message"].as_str().unwrap().contains("zzzz-none"), "{none}");
        let empty = c.call_raw("session.load", json!({ "session_id": "" }), |_| json!(null));
        assert_eq!(empty["error"]["data"]["code"], "invalid_arguments", "{empty}");
        s.shutdown();
    }
}

/// b16 SELECTOR-LEAK (should-fix): the grant is read at each call, so on one connection the
/// details follow `session.list` being revoked and granted again.
/// Reference test: reference implementation (one peer
/// before and after its access changes).
#[test]
fn selector_error_detail_follows_a_live_session_list_grant_on_the_same_connection() {
    let mut s = session(json!([]), r#"allow = { "session.load", "session.list" }"#);
    seed_ambiguous_sessions(&s);
    let mut c = s.peer();
    // granted: every detail is there; revoked: none of them anywhere in the error object
    let ambiguous = |c: &mut Sock| {
        let r = c.call_raw("session.load", json!({ "session_id": "abcd" }), |_| json!(null));
        assert_eq!(r["error"]["data"]["code"], "invalid_arguments", "{r}");
        r
    };
    let assert_detailed = |r: &Value, ctx: &str| {
        let whole = r["error"].to_string();
        for detail in ["Secret Alpha", "Secret Beta", "abcd1111", "abcd2222", "2 sessions"] {
            assert!(whole.contains(detail), "{ctx}: `{detail}` missing from {whole}");
        }
    };
    assert_detailed(&ambiguous(&mut c), "granted");
    let set = |s: &mut Session, allow: Value| {
        let sid = s.sid();
        let r = s.primary.call_raw(
            "options.set",
            json!({ "session_id": sid, "values": { "peer": { "allow": allow } } }),
            |_| json!(null),
        );
        assert!(r.get("error").is_none(), "{r}");
    };
    set(&mut s, json!(["session.load"]));
    assert_names_no_other_session(&ambiguous(&mut c), "revoked on the same connection");
    set(&mut s, json!(["session.load", "session.list"]));
    assert_detailed(&ambiguous(&mut c), "granted again");
    s.shutdown();
}

/// b16 SELECTOR-LEAK: a failure that is not the caller's selector (here the session service's
/// listing throws, through a replacement service) stays a masked `internal` error even for a
/// caller who may list — nothing of the failure crosses the wire.
/// Reference test: Kimi packages/acp-server/test/e2e-turn.test.ts:659.
#[test]
fn session_resume_lookup_failure_is_masked_internal_for_peers() {
    let home_marker = tempfile::tempdir().unwrap();
    let marker = home_marker.path().join("fault-reached");
    // a replacement session service (same stub as the RPC selector tests) whose list() fails
    let plugin = format!(
        "local MARKER = {:?}\n{}",
        marker.to_string_lossy(),
        include_str!("../../fixtures/failing_session_list.lua")
    );
    let s = session_env(
        json!([]),
        r#"allow = { "session.load", "session.list" }"#,
        false,
        &[],
        &[("session.lua", plugin.as_str())],
    );
    let mut c = s.peer();
    let r = c.call_raw("session.load", json!({ "session_id": "abcd" }), |_| json!(null));
    assert!(marker.exists(), "the injected listing failure was never reached: {r}");
    assert_eq!(r["error"]["data"]["code"], "internal", "{r}");
    let whole = r["error"].to_string();
    assert!(!whole.contains("SECRET-MARKER") && !whole.contains("/secret/"), "{whole}");
}

/// b25 ADMIT #2: an inbound peer message whose delivery fails internally (here the receiving
/// session is poisoned with a reason carrying a secret marker and a path) tells the sender only
/// a fixed reason — for an ordinary delivery and for `queue_only`. The compaction-busy test above
/// keeps the positive case (a caller-actionable reason is passed on).
/// Reference test: Kimi packages/acp-server/test/e2e-turn.test.ts:659.
#[test]
fn inbound_message_internal_delivery_failure_reaches_the_sender_masked() {
    let poison = r#"
        return {
            name = "poison-on-ready-test",
            apply = function(ctx)
                ctx:on("app/ready", function()
                    mare.task.fail_session("SECRET-MARKER-9d2 journal broken at /secret/journal/path")
                end)
            end,
        }
    "#;
    let mut s = session_env(json!([]), "", true, &[], &[("poison_on_ready.lua", poison)]);
    let _wd = watchdog(s.pid(), 45);
    let mut c = s.peer();
    for queue_only in [false, true] {
        let mut msg = message("trusted-admin");
        msg["queue_only"] = json!(queue_only);
        let r = c.call_raw("peer.message", msg, |_| json!(null));
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
        let t0 = Instant::now();
        let status = loop {
            assert!(t0.elapsed() < Duration::from_secs(30), "sender got no status");
            let m = c.read();
            if m["method"] == "peer.message_status" {
                break m;
            }
        };
        assert_eq!(status["params"]["status"], "rejected", "queue_only={queue_only}: {status}");
        let whole = status["params"].to_string();
        assert!(
            !whole.contains("SECRET-MARKER") && !whole.contains("/secret/"),
            "queue_only={queue_only}: the internal failure crossed to the sender: {whole}"
        );
        assert_eq!(
            status["params"]["reason"], "internal error (details are in the log)",
            "queue_only={queue_only}: {status}"
        );
    }
    drop(c);
    s.shutdown();
}

#[test]
fn primary_and_readonly_peer_audits_never_expose_rewrite_only_execution_data() {
    let mut session = session_env_fixture(
        json!([
            {"tool_calls":[{"name":"audit_effect","arguments":{"visible":"public request"}}]},{"text":"done"}
        ]),
        "allow = { 'events.subscribe' }",
        false,
        &[],
        &[],
        true,
    );
    let _wd = watchdog(session.pid(), 45);
    let sid = session.sid();
    let mut peer = session.peer();
    peer.call_raw("events.subscribe",json!({"session_id": sid, "include":["tool/before","tool/approval","approval/requested","approval/resolved","item/started","item/completed"]}),|_|json!(null));
    let started = session.primary.call_raw("agent.prompt", json!({"session_id": sid, "input":"go"}), |_| json!(null));
    assert!(started["error"].is_null(), "{started}");
    let id = session.primary.request("agent.wait", json!({"session_id": sid, "turn_id": started["result"]["turn_id"]}));
    let mut primary_audit = false;
    loop {
        let line = session.primary.read();
        assert!(
            !line.to_string().contains(APPROVAL_PRIVACY_SECRET),
            "primary observer leaked execution-only data: {line}"
        );
        assert_ne!(line["method"], "stream.failed", "{line}");
        if line["method"] == "event" && line["params"]["type"] == "approval/resolved" {
            primary_audit = true;
            assert_eq!(line["params"]["data"]["result"]["call"].as_object().unwrap().len(), 2, "{line}");
        }
        if line["id"] == id && line.get("method").is_none() {
            assert!(line.get("error").is_none() && line.to_string().contains("observed"), "{line}");
            break;
        }
    }
    let mut peer_audit = false;
    let mut start = false;
    loop {
        let line = peer.read();
        assert!(!line.to_string().contains(APPROVAL_PRIVACY_SECRET), "readonly peer leaked execution-only data");
        assert_ne!(line["method"], "stream.failed", "{line}");
        if line["method"] == "event" {
            let event = &line["params"];
            if event["type"] == "approval/resolved" {
                peer_audit = true;
                assert_eq!(event["data"]["result"]["call"].as_object().unwrap().len(), 2);
            }
            if event["type"] == "item/started" && event["data"]["item"]["type"] == "tool_call" {
                start = true;
                assert_eq!(event["data"]["item"]["arguments"], json!({"visible":"public request"}));
            }
            if event["type"] == "item/completed" && event["data"]["item"]["type"] == "tool_call" {
                break;
            }
        }
    }
    assert!(primary_audit && peer_audit && start);
    assert!(session._home.path().join("work/private-audit-observed").exists());
    drop(peer);
    session.shutdown();
}

/// Sends `text` as an inbound peer message, confirms it on the primary's dialog and returns the
/// sender's `peer.message_status`.
fn deliver_confirmed(s: &mut Session, text: &str, queue_only: bool) -> Value {
    let mut c = s.peer();
    let r = c.call_raw(
        "peer.message",
        json!({ "from": { "name": "alice" }, "text": text, "queue_only": queue_only }),
        |_| json!(null),
    );
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
    let t0 = Instant::now();
    loop {
        assert!(t0.elapsed() < Duration::from_secs(60), "sender got no status");
        let m = c.read();
        if m["method"] == "peer.message_status" {
            return m["params"].clone();
        }
    }
}

/// when the agent does not take an accepted message, the sender is told it
/// was rejected (with a reason), never "delivered".
#[test]
fn an_inbound_message_the_agent_refuses_is_not_reported_delivered() {
    let refuse = r#"return { name = "refuse-follow-up", inject = { "policy" }, apply = function(ctx)
        ctx.policy:voter("session/commit-ready", function() return { decision = "deny", reason = "refused by test" } end)
    end }"#;
    let mut s = session_env(json!([]), "", true, &[], &[("refuse-follow-up.lua", refuse)]);
    let _wd = watchdog(s.pid(), 60);
    let status = deliver_confirmed(&mut s, "queue this", true);
    assert_eq!(status["status"], "rejected", "{status}");
    // the refusal's own text stays in the log: an internal delivery failure reaches the sender
    // masked before reaching the sender, so the reason is the fixed one
    assert_eq!(status["reason"], "internal error (details are in the log)", "{status}");
    s.shutdown();
}

#[test]
fn inbound_message_across_session_switch_is_discarded_and_not_delivered() {
    let mut s = session_ui(json!([{ "text": "ok" }, { "text": "ok" }]), "", true);
    let _wd = watchdog(s.pid(), 45);
    let mut c = s.peer();
    for queue_only in [false, true] {
        let mut msg = message("trusted-admin");
        msg["queue_only"] = json!(queue_only);
        let r = c.call_raw("peer.message", msg, |_| json!(null));
        assert_eq!(r["result"]["status"], "held", "queue_only={queue_only}: {r}");
        let t0 = Instant::now();
        let ask = loop {
            assert!(t0.elapsed() < Duration::from_secs(30), "the receiving user was never asked");
            let m = s.primary.read();
            if m["method"] == "ui.select" {
                break m;
            }
        };

        let initial_sid = s.sid();
        // Switch away and close the initial session while ui.select is pending; then approve the stale prompt.
        let switched = s.primary.call_raw("session.create", json!({}), |_| json!(null));
        assert!(switched.get("error").is_none(), "queue_only={queue_only}: session.create failed: {switched}");
        let closed = s.primary.call_raw("session.close", json!({ "session_id": initial_sid }), |_| json!(null));
        assert!(closed.get("error").is_none(), "queue_only={queue_only}: session.close failed: {closed}");
        s.primary.send(json!({ "jsonrpc": "2.0", "id": ask["id"], "result": { "index": 0 } }));

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
            "queue_only={queue_only}: stale inbound message approved after session switch must be rejected: {status}"
        );
        assert!(
            status["params"]["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "queue_only={queue_only}: expected non-empty rejection reason on session switch: {status}"
        );

        let entries =
            s.primary.call_raw("session.list_items", json!({ "session_id": switched["result"]["session_id"] }), |_| {
                json!(null)
            });
        let messages = entries["result"]["data"]
            .as_array()
            .expect("session.list_items data array")
            .iter()
            .filter(|e| e["message"].is_object())
            .count();
        assert_eq!(
            messages, 0,
            "queue_only={queue_only}: discarded peer message must not be delivered to the new session: {entries}"
        );
    }
    s.shutdown();
}

#[cfg(unix)]
#[test]
fn events_list_peer_requires_grant_and_matches_primary() {
    let denied_session = session(json!([]), "");
    let _wd = watchdog(denied_session.pid(), 45);
    let mut ungranted_peer = denied_session.peer();
    let denied = ungranted_peer.request("events.list", json!({}));
    let denied_msg = loop {
        let m = ungranted_peer.read();
        if m.get("id").and_then(Value::as_u64) == Some(denied) {
            break m;
        }
    };
    assert_eq!(denied_msg["error"]["code"], -32601, "{denied_msg}");
    drop(ungranted_peer);
    denied_session.shutdown();

    let mut allowed_session = session(json!([]), "allow = { 'events.list' }");
    let _wd2 = watchdog(allowed_session.pid(), 45);
    let primary_list = allowed_session.primary.call_raw("events.list", json!({}), |_| json!(null));
    assert!(primary_list.get("error").is_none(), "{primary_list}");
    let mut peer = allowed_session.peer();
    let peer_list = peer.call_raw("events.list", json!({}), |_| json!(null));
    assert!(peer_list.get("error").is_none(), "{peer_list}");
    assert_eq!(peer_list["result"], primary_list["result"]);
    drop(peer);
    allowed_session.shutdown();
}

#[test]
fn peer_ui_allow_permits_readonly_and_refuses_mutating_and_lua_only_methods() {
    let mut s = session(json!([]), r#"allow = { "ui.*" }"#);
    let _wd = watchdog(s.pid(), 45);
    let mut peer = s.peer();

    // Prefix wildcard `"ui.*"` must NOT grant any `ui.*` operation under exact-name authorization.
    let denied = peer.call_raw("ui.list_markdown_presenters", json!({}), |_| json!(null));
    assert_eq!(denied["error"]["data"]["code"], "denied", "`ui.*` must not grant ui methods: {denied}");

    // Switch to exact operation names: read-only peer ops pass, primary-only *_markdown ops stay denied.
    let sid = s.sid();
    s.primary.call_raw(
        "options.set",
        json!({ "session_id": sid, "values": { "peer": { "allow": [
            "ui.list_markdown_presenters",
            "ui.transform_markdown",
            "ui.render_markdown",
            "ui.get_markdown",
            "ui.cancel_markdown",
            "ui.delete_markdown"
        ] } } }),
        |_| json!(null),
    );

    let presenters = peer.call_raw("ui.list_markdown_presenters", json!({}), |_| json!(null));
    assert!(presenters.get("error").is_none(), "peer ui.list_markdown_presenters should pass: {presenters}");
    assert!(presenters["result"]["data"].is_array(), "{presenters}");

    let transformed = peer.call_raw("ui.transform_markdown", json!({ "text": "hello" }), |_| json!(null));
    assert!(transformed.get("error").is_none(), "peer ui.transform_markdown should pass: {transformed}");
    assert_eq!(transformed["result"]["text"], "hello");

    let info = s.primary.call_raw("peer.info", json!({}), |_| json!(null));
    let _ = info;
    for (method, params) in [
        ("ui.render_markdown", json!({ "session_id": "s", "document_id": "d", "source": "x" })),
        ("ui.get_markdown", json!({ "session_id": "s", "document_id": "d" })),
        ("ui.cancel_markdown", json!({ "session_id": "s", "document_id": "d" })),
        ("ui.delete_markdown", json!({ "session_id": "s", "document_id": "d" })),
    ] {
        let resp = peer.call_raw(method, params, |_| json!(null));
        assert_eq!(resp["error"]["data"]["code"], "denied", "primary-only {method} must be denied: {resp}");
    }

    for lua_only in ["ui.widget", "ui.status"] {
        let resp = peer.call_raw(lua_only, json!({}), |_| json!(null));
        assert_eq!(err_code(&resp), METHOD_NOT_FOUND, "Lua-only {lua_only} must not be RPC-callable: {resp}");
    }

    drop(peer);
    s.shutdown();
}

#[cfg(unix)]
#[test]
fn peer_session_list_and_primary_session_mutations() {
    let mut s = session_persisted(json!([]), r#"allow = { "session.list" }"#);
    let _wd = watchdog(s.pid(), 45);
    let p_res = s.prompt_wait("hello-peer");
    assert!(p_res.get("error").is_none(), "{p_res}");

    let primary_list = s.primary.call_raw("session.list", json!({}), |_| json!(null));
    let sid = primary_list["result"]["data"][0]["session_id"]
        .as_str()
        .expect("primary session.list must include { data: [{ session_id }] }")
        .to_string();

    let mut peer = s.peer();
    let peer_list = peer.call_raw("session.list", json!({}), |_| json!(null));
    assert!(peer_list.get("error").is_none(), "peer must be allowed session.list: {peer_list}");
    assert_eq!(peer_list["result"], primary_list["result"]);

    for (method, params) in [
        ("session.update", json!({ "session_id": sid, "name": "hijack" })),
        ("session.label_item", json!({ "session_id": sid, "label": "hijack" })),
        ("session.archive", json!({ "session_id": sid })),
        ("session.unarchive", json!({ "session_id": sid })),
        ("session.delete", json!({ "session_id": sid })),
        ("session.search", json!({ "query": "hello" })),
        ("session.search_items", json!({ "session_id": sid, "query": "hello" })),
        ("session.get_item", json!({ "session_id": sid, "item_id": "invalid" })),
        ("session.trace", json!({ "session_id": sid })),
        ("session.trace_item", json!({ "session_id": sid, "item_id": "invalid" })),
        ("session.fork", json!({ "session_id": sid })),
        ("session.close", json!({ "session_id": sid })),
        ("session.rewind", json!({ "session_id": sid })),
        ("session.add_record", json!({ "session_id": sid, "kind": "x/y", "data": {} })),
    ] {
        let denied = peer.call_raw(method, params, |_| json!(null));
        assert_eq!(
            denied["error"]["data"]["code"], "denied",
            "primary-only {method} must be refused to peer even under session.*: {denied}"
        );
    }

    let spi = peer.call_raw("session.persist_message", json!({ "message": {} }), |_| json!(null));
    assert_eq!(spi["error"]["code"], -32601, "SPI method must stay non-RPC: {spi}");

    drop(peer);
    s.shutdown();
}

#[cfg(unix)]
#[test]
fn peer_session_list_is_scoped_to_current_project_and_rejects_escape() {
    let mut s = session_persisted(json!([]), r#"allow = { "session.list", "session.get" }"#);
    let _wd = watchdog(s.pid(), 45);
    let p_res = s.prompt_wait("host-proj");
    assert!(p_res.get("error").is_none(), "{p_res}");

    let work_dir = dunce::canonicalize(s._home.path().join("work")).unwrap();
    let other_dir = s._home.path().join("other-project");
    std::fs::create_dir_all(&other_dir).unwrap();
    let other_dir = dunce::canonicalize(&other_dir).unwrap();
    let other_faux = s._home.path().join("other-faux.json");
    std::fs::write(&other_faux, "[]").unwrap();
    let mut other_cmd = mare_cmd();
    other_cmd
        .args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", s._home.path())
        .env("MARE_FAUX_SCRIPT", &other_faux)
        .current_dir(&other_dir)
        .stderr(Stdio::null());
    let mut other_rpc = RpcClient::spawn_cmd(&mut other_cmd);
    let init_other = other_rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    assert!(init_other.get("error").is_none());
    let other_init_sid = init_other["result"]["session"]["session_id"].as_str().unwrap().to_string();
    let other_p = other_rpc.call_raw(
        "agent.prompt",
        json!({ "session_id": other_init_sid, "input": "other-proj-secret" }),
        |_| json!(null),
    );
    assert!(other_p.get("error").is_none(), "{other_p}");
    assert!(
        other_rpc
            .call_raw(
                "agent.wait",
                json!({ "session_id": other_init_sid, "turn_id": other_p["result"]["turn_id"] }),
                |_| json!(null)
            )
            .get("error")
            .is_none()
    );
    let other_listed = other_rpc.call_raw("session.list", json!({}), |_| json!(null));
    let other_sid = other_listed["result"]["data"][0]["session_id"].as_str().unwrap().to_string();
    other_rpc.shutdown_and_wait();

    let primary_all = s.primary.call_raw("session.list", json!({ "all": true }), |_| json!(null));
    let all_rows = primary_all["result"]["data"].as_array().unwrap();
    assert!(
        all_rows.iter().any(|r| r["session_id"] == other_sid),
        "primary all=true sees other project: {primary_all}"
    );

    let primary_other =
        s.primary.call_raw("session.list", json!({ "cwd": other_dir.to_string_lossy() }), |_| json!(null));
    let other_rows = primary_other["result"]["data"].as_array().unwrap();
    assert_eq!(other_rows.len(), 1, "{primary_other}");
    assert_eq!(other_rows[0]["session_id"], other_sid, "{primary_other}");

    let mut peer = s.peer();
    let peer_default = peer.call_raw("session.list", json!({}), |_| json!(null));
    assert!(peer_default.get("error").is_none(), "{peer_default}");
    let default_rows = peer_default["result"]["data"].as_array().unwrap();
    assert_eq!(default_rows.len(), 1, "{peer_default}");
    assert_ne!(default_rows[0]["session_id"], other_sid, "{peer_default}");

    let peer_same_cwd = peer.call_raw("session.list", json!({ "cwd": work_dir.to_string_lossy() }), |_| json!(null));
    assert!(peer_same_cwd.get("error").is_none(), "peer must be allowed same-project cwd: {peer_same_cwd}");
    assert_eq!(peer_same_cwd["result"], peer_default["result"]);

    let peer_all = peer.call_raw("session.list", json!({ "all": true }), |_| json!(null));
    assert_eq!(peer_all["error"]["data"]["code"], "denied", "peer session.list(all=true) must be denied: {peer_all}");

    let peer_other_cwd = peer.call_raw("session.list", json!({ "cwd": other_dir.to_string_lossy() }), |_| json!(null));
    assert_eq!(
        peer_other_cwd["error"]["data"]["code"], "denied",
        "peer session.list(cwd=other_project) must be denied: {peer_other_cwd}"
    );

    // Symlink and relative (`..`) `cwd` paths pointing outside the project must also be denied.
    let symlink_dir = s._home.path().join("work-link");
    std::os::unix::fs::symlink(&other_dir, &symlink_dir).unwrap();
    let peer_symlink_cwd =
        peer.call_raw("session.list", json!({ "cwd": symlink_dir.to_string_lossy() }), |_| json!(null));
    assert_eq!(
        peer_symlink_cwd["error"]["data"]["code"], "denied",
        "peer session.list(cwd=symlink) must be denied: {peer_symlink_cwd}"
    );
    let peer_dotdot_cwd = peer.call_raw("session.list", json!({ "cwd": "../other-project" }), |_| json!(null));
    assert_eq!(
        peer_dotdot_cwd["error"]["data"]["code"], "denied",
        "peer session.list(cwd=../other-project) must be denied: {peer_dotdot_cwd}"
    );

    // Same cwd_slug bucket with a different header cwd (or missing cwd) must be excluded for peers.
    let same_slug_dir = s._home.path().join("state/sessions").join(mare_store::cwd_slug(&work_dir));
    std::fs::write(
        same_slug_dir.join("2026-01-01T00-00-01_slug-collision.jsonl"),
        format!(
            "{}\n",
            json!({
                "type": "session",
                "version": mare_store::CURRENT_VERSION,
                "session_id": "slug-collision-other",
                "cwd": "/collision/other/project",
                "created_at_ms": 100
            })
        ),
    )
    .unwrap();
    std::fs::write(
        same_slug_dir.join("2026-01-01T00-00-02_missing-cwd.jsonl"),
        format!(
            "{}\n",
            json!({
                "type": "session",
                "version": mare_store::CURRENT_VERSION,
                "session_id": "missing-cwd-session",
                "created_at_ms": 101
            })
        ),
    )
    .unwrap();
    let peer_after_collision = peer.call_raw("session.list", json!({}), |_| json!(null));
    assert!(peer_after_collision.get("error").is_none(), "{peer_after_collision}");
    let collision_rows = peer_after_collision["result"]["data"].as_array().unwrap();
    assert!(
        !collision_rows
            .iter()
            .any(|r| r["session_id"] == "slug-collision-other" || r["session_id"] == "missing-cwd-session"),
        "peer session.list must filter out cwd_slug collisions and unknown-cwd rows: {peer_after_collision}"
    );

    // peer session.get must fail closed with `unknown_session` on cross-project, collision,
    // missing-cwd rows, and ambiguous cross-project prefixes.
    for target in [other_sid.as_str(), "slug-collision-other", "missing-cwd-session", "missing-cwd"] {
        let resp = peer.call_raw("session.get", json!({ "session_id": target }), |_| json!(null));
        assert_eq!(
            resp["error"]["data"]["code"], "unknown_session",
            "peer session.get({target}) must return unknown_session without leaking boundary: {resp}"
        );
    }

    drop(peer);
    s.shutdown();
}

#[test]
fn rpc_poisoned_diagnostic_reads_remain_available_after_session_poison() {
    let poison_plugin = r#"
return {
    name = "rpc-poison-trigger",
    inject = { "session", "commands" },
    apply = function(ctx)
        ctx.commands:register("poison-for-test", {
            description = "poisons the session barrier on demand",
            run = function()
                mare.task.fail_session("simulated-rpc-poison-failure")
            end,
        })
    end,
}
"#;
    let script = json!([
        { "text": "healthy initial reply" },
        { "text": "unreachable" }
    ]);
    let mut s = session_env_args(script, "", true, &[], &[("rpc_poison_trigger.lua", poison_plugin)], false, false);
    let _wd = watchdog(s.pid(), 45);

    let sid = s.sid();

    let ok_start =
        s.primary.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hello healthy" }), |_| json!(null));
    assert!(ok_start.get("error").is_none(), "{ok_start}");
    let ok_turn = s.primary.call_raw(
        "agent.wait",
        json!({ "session_id": sid, "turn_id": ok_start["result"]["turn_id"] }),
        |_| json!(null),
    );
    assert!(ok_turn.get("error").is_none(), "{ok_turn}");

    let rec_res = s.primary.call_raw(
        "session.add_record",
        json!({ "session_id": sid, "kind": "test/diag-record", "data": { "marker": "kept-before-poison" }, "sync": true }),
        |_| json!(null),
    );
    assert!(rec_res.get("error").is_none(), "{rec_res}");

    // Trip the session barrier outside tool execution context via /poison-for-test.
    let trip = s.primary.call_raw("commands.run", json!({ "line": "/poison-for-test" }), |_| json!(null));
    assert!(trip.get("error").is_none(), "{trip}");

    // Mutating operations must now be refused by the poison barrier.
    let blocked =
        s.primary.call_raw("session.update", json!({ "session_id": sid, "name": "blocked" }), |_| json!(null));
    assert!(blocked.get("error").is_some(), "mutating RPC must fail once poisoned: {blocked}");

    // All 5 read-only diagnostic RPCs marked `poisoned = "allow"` must succeed.
    let listed = s.primary.call_raw("session.list", json!({}), |_| json!(null));
    assert!(listed.get("error").is_none(), "session.list must work while poisoned: {listed}");
    assert!(
        listed["result"]["data"].as_array().unwrap().iter().any(|r| r["session_id"] == sid),
        "session.list missing active session while poisoned: {listed}"
    );

    let got = s.primary.call_raw("session.get", json!({ "session_id": sid }), |_| json!(null));
    assert!(got.get("error").is_none(), "session.get must work while poisoned: {got}");
    assert_eq!(got["result"]["session_id"], sid);

    let items = s.primary.call_raw("session.list_items", json!({ "session_id": sid }), |_| json!(null));
    assert!(items.get("error").is_none(), "session.list_items must work while poisoned: {items}");
    assert!(!items["result"]["data"].as_array().unwrap().is_empty(), "{items}");

    let recs =
        s.primary.call_raw("session.list_records", json!({ "session_id": sid, "kind": "test/diag-record" }), |_| {
            json!(null)
        });
    assert!(recs.get("error").is_none(), "session.list_records must work while poisoned: {recs}");
    let rec_arr = recs["result"]["data"].as_array().unwrap();
    assert_eq!(rec_arr.len(), 1, "expected diagnostic custom record while poisoned: {recs}");
    assert_eq!(rec_arr[0]["marker"], "kept-before-poison");

    let msgs = s.primary.call_raw("agent.list_messages", json!({ "session_id": sid }), |_| json!(null));
    assert!(msgs.get("error").is_none(), "agent.list_messages must work while poisoned: {msgs}");
    assert!(!msgs["result"]["data"].as_array().unwrap().is_empty(), "{msgs}");

    s.shutdown();
}

#[cfg(unix)]
#[test]
fn peer_compact_run_requires_grant_and_respects_lua_only_methods() {
    let mut denied_session = session(json!([]), "");
    let _wd = watchdog(denied_session.pid(), 45);
    let sid = denied_session.sid();
    let mut ungranted_peer = denied_session.peer();
    let denied = ungranted_peer.call_raw("compact.run", json!({ "session_id": sid }), |_| json!(null));
    assert_eq!(denied["error"]["data"]["code"], "denied", "ungranted peer compact.run must be refused: {denied}");
    drop(ungranted_peer);
    denied_session.shutdown();

    let mut allowed_session = session(json!([]), r#"allow = { "compact.run" }"#);
    let _wd2 = watchdog(allowed_session.pid(), 45);
    let sid2 = allowed_session.sid();
    let mut peer = allowed_session.peer();
    let res = peer.call_raw("compact.run", json!({ "session_id": sid2 }), |_| json!(null));
    assert!(res.get("error").is_none(), "granted peer compact.run should succeed: {res}");
    assert_eq!(res["result"]["status"], "nothing_to_compact", "{res}");

    for lua_only in ["compact.measure", "compact.threshold", "compact.cancel"] {
        let resp = peer.call_raw(lua_only, json!({}), |_| json!(null));
        assert_eq!(err_code(&resp), METHOD_NOT_FOUND, "Lua-only {lua_only} must not be RPC-callable: {resp}");
    }

    drop(peer);
    allowed_session.shutdown();
}
