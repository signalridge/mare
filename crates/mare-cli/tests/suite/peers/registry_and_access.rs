use super::*;

#[test]
fn registry_and_socket_are_private_and_cleaned_up() {
    let s = session(json!([]), "");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&s.sock).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&s.json_path).unwrap().permissions().mode() & 0o777, 0o600);
        let dir = s.sock.parent().unwrap();
        assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
    }
    // the endpoint and lock files, the registry file, their directory, and the pipe itself (its
    // DACL, read through a client handle: GENERIC_READ includes READ_CONTROL)
    #[cfg(windows)]
    {
        let lock = mare_binding::platform::pipe::lock_path(&s.sock);
        for p in [&s.sock, &lock, &s.json_path, &s.sock.parent().unwrap().to_path_buf()] {
            assert_eq!(mare_shared::private::check(p), Ok(()));
        }
        use std::os::windows::io::AsRawHandle;
        let pipe = connect(&s.sock);
        assert_eq!(mare_shared::private::check_handle(pipe.as_raw_handle()), Ok(()), "the pipe's DACL");
        // owned by this user's SID (also when elevated), what `mare peer send` verifies
        assert_eq!(mare_shared::private::check_owner_handle(pipe.as_raw_handle()), Ok(()), "the pipe's owner");
    }
    let entry: Value = serde_json::from_str(&std::fs::read_to_string(&s.json_path).unwrap()).unwrap();
    // "<dir>-<4 random hex digits>": not `pid & 0xff` (one collision in 256 sessions)
    let name = entry["name"].as_str().unwrap();
    let suffix = name.strip_prefix("work-").unwrap_or_else(|| panic!("{entry}"));
    assert!(suffix.len() == 4 && suffix.chars().all(|c| c.is_ascii_hexdigit()), "{entry}");
    assert!(entry["status"]["state"].is_string(), "{entry}");
    assert!(entry.get("token").is_none(), "no tokens in the registry: {entry}");
    let (json_path, sock) = (s.json_path.clone(), s.sock.clone());
    s.shutdown();
    assert!(!json_path.exists() && !sock.exists(), "registry not cleaned up");
    #[cfg(windows)]
    assert!(!mare_binding::platform::pipe::lock_path(&sock).exists(), "lock file not cleaned up");
}

/// `peer.info` through mare's own client (`IpcBackend::connect`: the pipe name from the endpoint
/// file, then the owner check).
#[cfg(windows)]
fn peer_info_via_mare_client(sock: &Path) -> Value {
    use mare_binding::platform::{Ipc, IpcBackend};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = <Ipc as IpcBackend>::connect(sock.to_str().unwrap()).await.unwrap();
        let (r, mut w) = tokio::io::split(conn);
        let mut r = tokio::io::BufReader::new(r);
        let calls = [
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0", "capabilities": {} } }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "peer.info", "params": {} }),
        ];
        for c in calls {
            w.write_all(format!("{c}\n").as_bytes()).await.unwrap();
        }
        let read = async {
            loop {
                let mut line = String::new();
                assert!(r.read_line(&mut line).await.unwrap() > 0, "connection closed");
                let v: Value = serde_json::from_str(&line).unwrap();
                if v["id"] == 2 {
                    return v;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(30), read).await.expect("no peer.info answer")
    })
}

/// The pipe's name is random (not derived from the socket path, which anyone can predict) and
/// recorded in the endpoint file, which stays readable — the lock is a separate file — so a client
/// finds the session through it.
#[cfg(windows)]
#[test]
fn a_client_finds_the_random_pipe_through_the_endpoint_file() {
    let s = session(json!([]), "");
    let name = std::fs::read_to_string(&s.sock).unwrap();
    let hex = name.strip_prefix(r"\\.\pipe\mare-").unwrap_or_else(|| panic!("{name}"));
    assert!(hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit()), "{name}");
    assert_ne!(name, mare_binding::platform::pipe_name(&s.sock), "derived from the path");
    assert!(mare_binding::platform::pipe::lock_path(&s.sock).exists());
    let info = peer_info_via_mare_client(&s.sock);
    assert_eq!(info["result"]["pid"], s.pid(), "{info}");
    s.shutdown();
}

/// Another process already holds the name the session draws first (forced by the debug-build hook
/// `MARE_TEST_PIPE_FIRST_NAME`; the squatter's pipe is created here, with the default DACL rather
/// than the session's): the session draws another name, listens, and is reachable through the
/// endpoint file; the squatter's pipe is untouched.
#[cfg(windows)]
#[test]
fn a_squatted_pipe_name_does_not_keep_the_session_from_listening() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let squatted = format!(r"\\.\pipe\mare-{}", uuid::Uuid::new_v4().simple());
    let squat = {
        let _g = rt.enter();
        tokio::net::windows::named_pipe::ServerOptions::new().first_pipe_instance(true).create(&squatted).unwrap()
    };
    let s = session_env(json!([]), "", false, &[("MARE_TEST_PIPE_FIRST_NAME", &squatted)], &[]);
    let name = std::fs::read_to_string(&s.sock).unwrap();
    assert!(name.starts_with(r"\\.\pipe\mare-") && name != squatted, "{name}");
    let info = peer_info_via_mare_client(&s.sock);
    assert_eq!(info["result"]["pid"], s.pid(), "{info}");
    s.shutdown();
    drop(squat);
}

#[test]
fn a_peer_never_takes_over_the_sessions_ui() {
    let s = session(json!([]), "");
    let mut c = s.client();
    let r = c.call_raw(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "ui": true, "approvals": true } }),
        |_| json!(null),
    );
    assert_eq!(r["result"]["capabilities"]["callbacks"]["ui"], false, "{r}");
    assert_eq!(r["result"]["capabilities"]["callbacks"]["approvals"], false, "{r}");
    s.shutdown();
}

#[test]
fn peer_gets_only_the_default_methods() {
    let s = session(json!([]), "");
    let mut c = s.peer();
    for m in
        ["prompt", "options.get", "options.set", "config.describe", "state.get", "models.list", "ui.select.highlight"]
    {
        assert_eq!(err_code(&c.call_raw(m, json!({ "text": "x" }), |_| json!(null))), METHOD_NOT_FOUND, "{m}");
    }
    let reg_denied = c.call_raw(
        "tools.register",
        json!({ "name": "x", "description": "d", "parameters": { "type": "object" }, "callbacks": ["execute"] }),
        |_| json!(null),
    );
    assert_eq!(reg_denied["error"]["data"]["code"], json!("denied"), "{reg_denied}");
    let hook_denied =
        c.call_raw("hooks.subscribe", json!({ "event": "status/changed", "callbacks": ["handle"] }), |_| json!(null));
    assert_eq!(hook_denied["error"]["data"]["code"], json!("denied"), "{hook_denied}");
    let denied_create = c.call_raw("session.create", json!({}), |_| json!(null));
    assert_eq!(denied_create["error"]["data"]["code"], "denied", "{denied_create}");
    let info = c.call_raw("peer.info", json!({}), |_| json!(null));
    let allowed: Vec<&str> = info["result"]["allowed"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(allowed.contains(&"peer.message") && !allowed.contains(&"prompt"), "{info}");
    assert_eq!(info["result"]["pid"], s.pid());
    let st = c.call_raw("status.get", json!({}), |_| json!(null));
    assert!(st["result"]["state"].is_string(), "{st}");
    s.shutdown();
}

/// A peer allowed `session.create` gets a new session: the session service's own `session/before-switch`
/// decision runs inside the peer's authorized operation (policy.decide is the point owner's check,
/// access = "peer"). The allow stays narrow: a method it does not name is still refused, and a peer
/// without the allow is refused session.create itself (peer_gets_only_the_default_methods).
#[test]
fn a_peer_allowed_session_create_starts_a_session_and_nothing_else() {
    let mut s = session(json!([]), r#"allow = { "session.create" }"#);
    let before = s.sid();
    let mut c = s.peer();
    let r = c.call_raw("session.create", json!({}), |_| json!(null));
    assert!(r.get("error").is_none(), "the allowed session.create was refused: {r}");
    // a new session, not the current one handed back
    let after = r["result"]["session_id"].as_str().expect("a session id").to_string();
    assert_ne!(after, before, "session.create left the session as it was: {r}");
    assert_eq!(
        err_code(&c.call_raw("options.set", json!({ "key": "x", "value": 1 }), |_| json!(null))),
        METHOD_NOT_FOUND
    );
    s.shutdown();
}

#[test]
fn explicit_allow_opens_methods_and_disconnect_disposes_them() {
    let call = json!({ "tool_calls": [{ "name": "peer_tool", "arguments": { "q": 1 } }] });
    let mut s = session(json!([call, { "text": "one" }, call, { "text": "two" }]), r#"allow = { "tools.register" }"#);
    let mut a = s.peer_with(json!({ "callbacks": true }));
    let reg = a.call_raw(
        "tools.register",
        json!({ "session_id": s.sid(), "name": "peer_tool", "description": "answers", "parameters": { "type": "object" }, "callbacks": ["execute"] }),
        |_| json!(null),
    );
    assert!(reg.get("error").is_none(), "{reg}");
    // still not allowed: what the session didn't list
    assert_eq!(
        a.call_raw("agent.prompt", json!({ "session_id": "s", "input": "x" }), |_| json!(null))["error"]["data"]["code"],
        "denied"
    );
    assert_eq!(err_code(&a.call_raw("options.get", json!({}), |_| json!(null))), METHOD_NOT_FOUND);
    assert_eq!(err_code(&a.call_raw("config.describe", json!({}), |_| json!(null))), METHOD_NOT_FOUND);

    // the primary's prompt calls A's tool; A answers the callback.call over its socket
    let a_thread = std::thread::spawn(move || {
        let m = loop {
            let m = a.read();
            if m.get("id").is_some() {
                break m;
            }
        };
        assert_eq!(m["method"], "callback.call", "{m}");
        assert_eq!(m["params"]["callback"], "execute", "{m}");
        a.send(json!({ "jsonrpc": "2.0", "id": m["id"], "result": { "content": "from-peer" } }));
        a
    });
    let r = s.prompt_wait("go");
    let a = a_thread.join().unwrap();
    assert!(r["result"].to_string().contains("from-peer"), "{r}");

    drop(a);
    // sleep-ok: allow server loop to process peer disconnect and dispose owned tools
    std::thread::sleep(Duration::from_millis(300));
    let r = s.prompt_wait("again");
    let dump = r["result"].to_string();
    assert!(!dump.contains("from-peer") && dump.to_lowercase().contains("peer_tool"), "{r}");
    s.shutdown();
}

#[test]
fn peer_tool_update_is_isolated_to_owning_connection() {
    let call = json!({ "tool_calls": [{ "name": "primary_tool", "arguments": {} }] });
    let mut s = session(json!([call, { "text": "done" }]), "");
    let caps = s.primary.call_raw(
        "connection.set_capabilities",
        json!({ "capabilities": { "callbacks": true, "ui": true, "approvals": false, "elicitation": false } }),
        |_| json!(null),
    );
    assert!(caps.get("error").is_none(), "{caps}");
    let status = s.primary.call_raw("status.get", json!({}), |_| json!(null));
    let sid = status["result"]["session"]["session_id"].as_str().unwrap().to_string();
    let reg = s.primary.call_raw(
        "tools.register",
        json!({ "name": "primary_tool", "description": "primary", "parameters": { "type": "object" }, "callbacks": ["execute"] }),
        |_| json!(null),
    );
    assert!(reg.get("error").is_none(), "{reg}");

    let mut peer = s.peer_with(json!({ "callbacks": true }));
    let prompt_id = s.primary.request("agent.prompt", json!({ "session_id": sid, "input": "go" }));
    let mut events = Vec::new();
    let call_msg = loop {
        let m = s.primary.read();
        if m["method"] == "event" {
            events.push(m["params"].clone());
        } else if m["method"] == "callback.call" {
            assert_eq!(m["params"]["callback"], "execute", "{m}");
            break m;
        } else if m["id"] == prompt_id {
            assert_eq!(m["result"]["status"], "started", "{m}");
        }
    };
    let cb_req_id = call_msg["id"].clone();

    // A different connection MUST NOT be able to inject `callback.update` for primary's in-flight call.
    peer.notify("callback.update", json!({ "id": cb_req_id, "partial": { "output": "spoofed-by-peer" } }));
    // Synchronize the peer socket so its notification has been processed by the server before primary updates.
    let info = peer.call_raw("peer.info", json!({}), |_| json!(null));
    assert!(info.get("error").is_none(), "{info}");

    s.primary.notify("callback.update", json!({ "id": cb_req_id, "partial": { "output": "from-primary" } }));
    loop {
        let m = s.primary.read();
        if m["method"] == "event" {
            let p = m["params"].clone();
            events.push(p.clone());
            if p["type"] == "item/delta" && p["data"]["kind"] == "tool_output" {
                break;
            }
        }
    }
    s.primary.send(json!({ "jsonrpc": "2.0", "id": call_msg["id"], "result": { "content": "ok" } }));
    loop {
        let m = s.primary.read();
        if m["method"] == "event" {
            let p = m["params"].clone();
            events.push(p.clone());
            if p["type"] == "turn/completed" {
                break;
            }
        }
    }
    let outputs: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "item/delta" && e["data"]["kind"] == "tool_output")
        .map(|e| e["data"]["partial"]["output"].as_str().unwrap())
        .collect();
    assert_eq!(outputs, vec!["from-primary"], "{events:?}");
    drop(peer);
    s.shutdown();
}

/// Switching the active session in multi-session `--rpc --peer` mode must refresh both `cwd`
/// and the wire projector hook in `<state>/peers/<pid>.json` from the new session's VM, and
/// closing the previous session must leave the server responsive.
#[test]
fn peer_registry_refreshes_cwd_and_wire_projector_on_session_switch_and_stays_responsive_after_close() {
    const OTHER_SECRET: &str = "PEER_SWITCH_SECRET_TOKEN_98765";
    let plugin = format!(
        r#"return {{
    name = "peer_switch_probe",
    apply = function(ctx)
        ctx:on("app/ready", function()
            if string.find(mare.cwd, "other_project", 1, true) then
                KEEP_SWITCH_PROJ = mare.projection.register("{OTHER_SECRET}")
                mare.peers.publish("switch_probe", {{ marker = "other", secret = "{OTHER_SECRET}" }})
            else
                mare.peers.publish("switch_probe", {{ marker = "initial" }})
            end
        end)
    end,
}}"#
    );
    let mut s = session_env(json!([]), "", false, &[], &[("peer_switch_probe.lua", &plugin)]);
    let _wd = watchdog(s.pid(), 60);
    let first_sid = s.sid();

    let initial = registry_until(&s, "initial published marker", |e| {
        e.pointer("/plugins/switch_probe/marker").and_then(Value::as_str) == Some("initial")
    });
    assert!(
        initial["cwd"].as_str().unwrap_or_default().ends_with("work"),
        "expected initial peer cwd in work: {initial}"
    );

    let other_dir = s._home.path().join("other_project");
    std::fs::create_dir_all(&other_dir).unwrap();
    let created = s.primary.call_raw("session.create", json!({ "cwd": other_dir.to_string_lossy() }), |_| json!(null));
    assert!(created.get("error").is_none(), "session.create failed: {created}");

    let switched = registry_until(&s, "switched session marker and cwd", |e| {
        e.pointer("/plugins/switch_probe/marker").and_then(Value::as_str) == Some("other")
    });
    let expected_cwd = dunce::canonicalize(&other_dir).unwrap_or_else(|_| other_dir.clone());
    let actual_cwd_str = switched["cwd"].as_str().unwrap_or_default();
    let actual_cwd = dunce::canonicalize(actual_cwd_str).unwrap_or_else(|_| PathBuf::from(actual_cwd_str));
    assert_eq!(actual_cwd, expected_cwd, "peer registry cwd did not refresh on session switch: {switched}");

    let raw_json = std::fs::read_to_string(&s.json_path).unwrap();
    assert!(
        !raw_json.contains(OTHER_SECRET),
        "new session's wire projector must redact secret in peer registry: {raw_json}"
    );
    let redacted = switched.pointer("/plugins/switch_probe/secret").and_then(Value::as_str).unwrap_or_default();
    assert!(!redacted.is_empty() && !redacted.contains(OTHER_SECRET), "unexpected projected secret: {switched}");

    // Closing the old session must not spin or freeze the `--rpc --peer` runtime.
    let closed = s.primary.call_raw("session.close", json!({ "session_id": first_sid }), |_| json!(null));
    assert!(closed.get("error").is_none(), "session.close failed: {closed}");
    let status = s.primary.call_raw("status.get", json!({}), |_| json!(null));
    assert!(status.get("error").is_none(), "status.get failed after session.close: {status}");
    s.shutdown();
}
