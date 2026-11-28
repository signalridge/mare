//! Unknown sessions and process termination as seen by an ACP client.

use super::*;

/// ACP: a session-scoped request naming a session that does not exist or is not active is
/// `-32002` Resource not found, with the session id in `data`.
#[test]
fn acp_requests_for_an_unknown_session_are_resource_not_found() {
    let (mut rpc, home) = acp(json!([{ "text": "ok" }]));
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let sid = "no-such-session";
    for (method, params) in [
        ("session/resume", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] })),
        ("session/load", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] })),
        ("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "x" }] })),
        ("session/set_config_option", json!({ "sessionId": sid, "configId": "model", "value": "faux/echo" })),
        ("session/close", json!({ "sessionId": sid })),
    ] {
        let res = rpc.call_raw(method, params, |_| json!(null));
        assert_eq!(res["error"]["code"], -32002, "{method}: {res}");
        assert_eq!(res["error"]["data"]["sessionId"], sid, "{method}: {res}");
        assert!(
            res["error"]["data"]["message"].as_str().is_some_and(|m| m.contains(sid)),
            "{method}: the explanation stays in data: {res}"
        );
    }
    rpc.kill_and_wait();
}

/// `MARE_HOME/config/init.lua` that writes `marker` from an `app/shutdown` handler.
#[cfg(unix)]
fn shutdown_marker(home: &std::path::Path) -> std::path::PathBuf {
    let marker = home.join("shutdown-ran");
    let config = home.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        format!(
            r#"mare.on("app/shutdown", function()
  local f = assert(io.open({marker:?}, "w"))
  f:write("ok")
  f:close()
end)
"#
        ),
    )
    .unwrap();
    marker
}

/// Send SIGTERM and wait for the exit; returns the status and how long it took.
#[cfg(unix)]
fn sigterm_and_wait(rpc: &mut RpcClient) -> (std::process::ExitStatus, std::time::Duration) {
    use std::time::{Duration, Instant};
    let child = rpc.child.as_mut().expect("child");
    let t0 = Instant::now();
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let deadline = t0 + Duration::from_secs(15);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            return (status, t0.elapsed());
        }
        assert!(Instant::now() < deadline, "mare acp did not exit after SIGTERM");
        // sleep-ok: polling child.try_wait() after SIGTERM with a 15s deadline
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// An idle `mare acp` exits on SIGTERM right away, through the normal shutdown (plugins see
/// app/shutdown), instead of waiting out the forced-exit deadline.
#[cfg(unix)]
#[test]
fn acp_sigterm_while_idle_exits_promptly_through_normal_shutdown() {
    let home = tempfile::tempdir().unwrap();
    let marker = shutdown_marker(home.path());
    let mut rpc = acp_in_home(home.path(), json!([{ "text": "ok" }]), &[], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    assert!(s["result"]["sessionId"].is_string(), "{s}");

    let (status, took) = sigterm_and_wait(&mut rpc);
    // the forced exit fires at 5 s and skips app/shutdown; well under it means the normal path
    assert!(took < std::time::Duration::from_millis(2500), "idle SIGTERM took {took:?}");
    assert!(marker.exists(), "app/shutdown must run before exit");
    assert_eq!(status.code(), Some(143), "{status:?}");
}

/// With a turn in flight, SIGTERM cancels it and still shuts down through app/shutdown.
#[cfg(unix)]
#[test]
fn acp_sigterm_during_a_turn_cancels_it_and_shuts_down_normally() {
    let home = tempfile::tempdir().unwrap();
    let marker = shutdown_marker(home.path());
    let mut rpc = acp_in_home(home.path(), json!([{ "text": "x".repeat(4000), "delay_ms": 20 }]), &[], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();
    let _ = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "long" }] }));
    loop {
        let m = rpc.read();
        if m["params"]["update"]["content"]["text"].as_str().is_some_and(|t| t.starts_with('x')) {
            break;
        }
    }

    let (status, took) = sigterm_and_wait(&mut rpc);
    assert!(took < std::time::Duration::from_millis(4000), "SIGTERM during a turn took {took:?}");
    assert!(marker.exists(), "app/shutdown must run before exit");
    assert_eq!(status.code(), Some(143), "{status:?}");
}

/// ACP `session/list` and `session/resume` must include user `fork` sessions (which carry
/// `parent_session_id` in their header) and exclude sessions archived via `{type="archive"}`.
#[test]
fn acp_lists_and_resumes_fork_sessions_and_hides_archived_sessions() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(ws.path()).unwrap();
    let cwd_str = cwd.to_string_lossy().to_string();

    let sessions_dir = home.path().join("state/sessions/proj-test");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let write_sess = |file: &str, lines: &[serde_json::Value]| {
        let mut content = String::new();
        for l in lines {
            content.push_str(&l.to_string());
            content.push('\n');
        }
        std::fs::write(sessions_dir.join(file), content).unwrap();
    };
    let main_id = "aaaaaaaa-1111-2222-3333-444444444444";
    let fork_id = "bbbbbbbb-1111-2222-3333-444444444444";
    let arch_id = "cccccccc-1111-2222-3333-444444444444";
    let unarch_id = "dddddddd-1111-2222-3333-444444444444";
    let agent_id = "eeeeeeee-1111-2222-3333-444444444444";
    write_sess(
        "main.jsonl",
        &[
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":main_id,"cwd":cwd_str,"name":"Main Session","created_at_ms":1700000001000i64}),
            json!({"type":"message","entry_id":"m1","created_at_ms":1700000001001i64,"message":{"role":"user","content":[{"type":"text","text":"hi"}]}}),
        ],
    );
    write_sess(
        "fork.jsonl",
        &[
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":fork_id,"parent_session_id":main_id,"cwd":cwd_str,"name":"Forked Session","created_at_ms":1700000002000i64}),
            json!({"type":"message","entry_id":"m1","created_at_ms":1700000002001i64,"message":{"role":"user","content":[{"type":"text","text":"hi fork"}]}}),
        ],
    );
    write_sess(
        "archived.jsonl",
        &[
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":arch_id,"cwd":cwd_str,"name":"Archived Session","created_at_ms":1700000003000i64}),
            json!({"type":"message","entry_id":"m1","created_at_ms":1700000003001i64,"message":{"role":"user","content":[{"type":"text","text":"bye"}]}}),
            json!({"type":"archive","archived":true,"created_at_ms":1700000003002i64}),
        ],
    );
    write_sess(
        "unarchived.jsonl",
        &[
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":unarch_id,"cwd":cwd_str,"name":"Unarchived Session","created_at_ms":1700000004000i64}),
            json!({"type":"message","entry_id":"m1","created_at_ms":1700000004001i64,"message":{"role":"user","content":[{"type":"text","text":"back"}]}}),
            json!({"type":"archive","archived":true,"created_at_ms":1700000004002i64}),
            json!({"type":"archive","archived":false,"created_at_ms":1700000004003i64}),
        ],
    );
    let agent_dir = sessions_dir.join("agents/run-1");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("subagent.jsonl"),
        format!(
            "{}\n{}\n",
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":agent_id,"parent_session_id":main_id,"cwd":cwd_str,"name":"Subagent Session","created_at_ms":1700000005000i64}),
            json!({"type":"message","entry_id":"m1","created_at_ms":1700000005001i64,"message":{"role":"user","content":[{"type":"text","text":"child"}]}}),
        ),
    )
    .unwrap();

    let mut rpc = acp_in_home(home.path(), json!([{ "text": "ok" }]), &[], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    let listed = rpc.call_raw("session/list", json!({ "cwd": cwd_str }), |_| json!(null));
    let ids: Vec<String> = listed["result"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("expected sessions array: {listed}"))
        .iter()
        .filter_map(|s| s["sessionId"].as_str().map(str::to_string))
        .collect();
    assert!(ids.contains(&main_id.to_string()), "main session must be listed: {ids:?}");
    assert!(ids.contains(&fork_id.to_string()), "user fork session must be listed by ACP: {ids:?}");
    assert!(ids.contains(&unarch_id.to_string()), "unarchived session must be listed by ACP: {ids:?}");
    assert!(!ids.contains(&arch_id.to_string()), "archived session must NOT be listed by ACP: {ids:?}");
    assert!(!ids.contains(&agent_id.to_string()), "subagent session must NOT be listed by ACP: {ids:?}");

    let res_fork =
        rpc.call_raw("session/resume", json!({ "sessionId": fork_id, "cwd": cwd_str, "mcpServers": [] }), |_| {
            json!(null)
        });
    assert!(res_fork.get("error").is_none(), "ACP must be able to resume user fork session: {res_fork}");

    let res_arch =
        rpc.call_raw("session/resume", json!({ "sessionId": arch_id, "cwd": cwd_str, "mcpServers": [] }), |_| {
            json!(null)
        });
    assert_eq!(res_arch["error"]["code"], -32002, "archived session must not be resumable via ACP: {res_arch}");

    let res_agent =
        rpc.call_raw("session/resume", json!({ "sessionId": agent_id, "cwd": cwd_str, "mcpServers": [] }), |_| {
            json!(null)
        });
    assert_eq!(res_agent["error"]["code"], -32002, "subagent session must not be resumable via ACP: {res_agent}");

    rpc.kill_and_wait();
}

/// An in-place session rekey triggered by a plugin during a turn must keep the client-facing ACP
/// `sessionId` fixed (matching kimi-code), must not abort the in-flight prompt or leak its
/// `in_flight_prompts` slot, and must keep streaming updates and accepting follow-up prompts and
/// `session/close` under the original ACP `sessionId`.
#[test]
fn acp_in_place_session_rekey_preserves_client_session_id_and_inflight_prompt() {
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("rekeyed-session-id");
    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("rekey-test.lua"),
        format!(
            r#"return {{
  name = "rekey-test",
  inject = {{ "session" }},
  apply = function(ctx)
    local rekeyed = false
    ctx:on("agent/settled", function(ev)
      if rekeyed or (ev and ev.busy) then return end
      rekeyed = true
      ctx.session:switch({{ kind = "new" }})
      local info = ctx.session:info()
      local f = assert(io.open({marker:?}, "w"))
      f:write(tostring(info and info.session_id or ""))
      f:close()
    end)
  end,
}}
"#
        ),
    )
    .unwrap();

    let mut rpc = acp_in_home(
        home.path(),
        json!([
            { "text": "turn-1-before-rekey" },
            { "text": "turn-2-after-rekey" }
        ]),
        &[],
        &[],
    );
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("session/new failed: {s}")).to_string();

    // Turn 1 triggers an in-place rekey via `agent/settled` before `host.prompt()` returns.
    let id1 =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "first" }] }));
    let mut turn1_updates = Vec::new();
    let p1 = loop {
        let m = rpc.read();
        if m["method"] == "session/update" {
            turn1_updates.push(m);
        } else if m["id"] == id1 {
            break m;
        }
    };
    assert_eq!(p1["result"]["stopReason"], "end_turn", "in-place rekey must not cancel in-flight prompt: {p1}");
    assert!(
        turn1_updates.iter().all(|u| u["params"]["sessionId"] == sid),
        "every turn 1 update must use original ACP sessionId {sid}: {turn1_updates:?}"
    );
    let turn1_text: String =
        turn1_updates.iter().filter_map(|u| u["params"]["update"]["content"]["text"].as_str()).collect();
    assert!(turn1_text.contains("turn-1-before-rekey"), "missing turn 1 text under {sid}: {turn1_text:?}");
    let new_internal_sid = std::fs::read_to_string(&marker).expect("rekey plugin must have run").trim().to_string();
    assert!(
        !new_internal_sid.is_empty() && new_internal_sid != sid,
        "expected in-place rekey to a new id: orig={sid}, new={new_internal_sid}"
    );

    // Turn 2 using the original ACP sessionId must succeed (no -32002 and no leaked in_flight_prompts).
    let id2 =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "second" }] }));
    let mut turn2_updates = Vec::new();
    let p2 = loop {
        let m = rpc.read();
        if m["method"] == "session/update" {
            turn2_updates.push(m);
        } else if m["id"] == id2 {
            break m;
        }
    };
    assert_eq!(p2["result"]["stopReason"], "end_turn", "follow-up prompt under original ACP sessionId failed: {p2}");
    assert!(
        turn2_updates.iter().all(|u| u["params"]["sessionId"] == sid),
        "every turn 2 update must use original ACP sessionId {sid}: {turn2_updates:?}"
    );
    let turn2_text: String =
        turn2_updates.iter().filter_map(|u| u["params"]["update"]["content"]["text"].as_str()).collect();
    assert!(turn2_text.contains("turn-2-after-rekey"), "missing turn 2 text under {sid}: {turn2_text:?}");

    // Closing via the original ACP sessionId must succeed.
    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("error").is_none(), "session/close with original ACP sessionId failed: {closed}");

    rpc.kill_and_wait();
}

/// ACP `session/list` must populate `updatedAt` from logical last activity time (RFC 3339) and
/// order by logical activity rather than file mtime (touching/rewriting an older session's file
/// without new messages must not bump its `updatedAt` or sort position).
#[test]
fn acp_session_list_returns_rfc3339_updated_at_and_ignores_file_mtime_drift() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(ws.path()).unwrap();
    let cwd_str = cwd.to_string_lossy().to_string();

    let sessions_dir = home.path().join("state/sessions/proj-updated-at");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let older_id = "11111111-1111-2222-3333-444444444444";
    let newer_id = "22222222-1111-2222-3333-444444444444";
    let empty_id = "33333333-1111-2222-3333-444444444444";

    let newer_path = sessions_dir.join("newer.jsonl");
    let older_path = sessions_dir.join("older.jsonl");
    let empty_path = sessions_dir.join("empty.jsonl");

    std::fs::write(
        &newer_path,
        format!(
            "{}\n{}\n",
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":newer_id,"cwd":cwd_str,"name":"Newer","created_at_ms":1700000010000i64}),
            json!({"type":"message","entry_id":"m1","created_at_ms":1700000020500i64,"message":{"role":"user","content":[{"type":"text","text":"latest activity"}]}}),
        ),
    )
    .unwrap();
    std::fs::write(
        &empty_path,
        format!(
            "{}\n",
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":empty_id,"cwd":cwd_str,"name":"No Messages","created_at_ms":1700000015000i64}),
        ),
    )
    .unwrap();
    // Write the older-activity session LAST (and append a non-message name record) so its file
    // mtime is strictly newer than `newer.jsonl` while its logical activity remains earlier.
    std::thread::sleep(std::time::Duration::from_millis(25));
    std::fs::write(
        &older_path,
        format!(
            "{}\n{}\n{}\n",
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":older_id,"cwd":cwd_str,"name":"Older","created_at_ms":1700000001000i64}),
            json!({"type":"message","entry_id":"m1","created_at_ms":1700000005000i64,"message":{"role":"user","content":[{"type":"text","text":"older activity"}]}}),
            json!({"type":"name","name":"Older Renamed","created_at_ms":1700000099000i64}),
        ),
    )
    .unwrap();

    let mut rpc = acp_in_home(home.path(), json!([{ "text": "ok" }]), &[], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    let listed = rpc.call_raw("session/list", json!({ "cwd": cwd_str }), |_| json!(null));
    let sessions =
        listed["result"]["sessions"].as_array().unwrap_or_else(|| panic!("expected sessions array: {listed}"));
    assert_eq!(sessions.len(), 3, "expected 3 sessions: {sessions:?}");
    let got: Vec<(&str, Option<&str>)> =
        sessions.iter().map(|s| (s["sessionId"].as_str().unwrap_or(""), s["updatedAt"].as_str())).collect();
    assert_eq!(
        got,
        vec![
            (newer_id, Some("2023-11-14T22:13:40.5Z")),
            (empty_id, Some("2023-11-14T22:13:35Z")),
            (older_id, Some("2023-11-14T22:13:25Z")),
        ],
        "session/list must return logical RFC 3339 updatedAt and order descending by activity: {sessions:?}"
    );

    rpc.kill_and_wait();
}
