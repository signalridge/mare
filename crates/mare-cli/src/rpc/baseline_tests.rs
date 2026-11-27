//! baseline tests: mid-stream cut + concurrent turn parity, 60s idle expiry with paused
//! clock, supersede/close release, paging node budgets, and cross-session lag isolation.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{BufReader, ReadHalf, WriteHalf};
use tokio::sync::Notify;

use super::tests::{HOST_LOCK, rpc_read, rpc_read_reply_with_pre as read_reply_and_pre, rpc_send};
use super::*;

async fn open_test_conn(
    host: &Arc<Host>,
    primary: bool,
    buf_bytes: usize,
) -> (
    BufReader<ReadHalf<tokio::io::DuplexStream>>,
    WriteHalf<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<()>,
    String,
) {
    let (client, server) = tokio::io::duplex(buf_bytes);
    let (sr, sw) = tokio::io::split(server);
    let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, primary, std::future::pending()));
    let (cr, mut cw) = tokio::io::split(client);
    let mut cr = BufReader::new(cr);
    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}})).await;
    let (init, _) = read_reply_and_pre(&mut cr, 1).await;
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("missing session id in {init}"))
        .to_string();
    (cr, cw, server_task, sid)
}

fn set_peer_allow(host: &Host, allow: &[&str]) {
    let mut config = host.kernel.config();
    config["peer"]["allow"] = json!(allow);
    host.kernel.set_config(config);
}

fn seed_private_baseline(host: &Host) {
    let lua = host.kernel.lua();
    let setup: mlua::Function = lua
        .load(
            r#"
            local session, agent = ...
            agent:set_queued({ steer = { { input_id = "q-private", text = "PRIVATE_QUEUE_TEXT" } } })
            session:persist_message({ message = { role = "user", content = { { type = "text", text = "PRIVATE_ENTRY_TEXT" } } } })
            "#,
        )
        .into_function()
        .unwrap();
    setup.call::<()>((host.kernel.service("session").unwrap(), host.kernel.service("agent").unwrap())).unwrap();
}

// Verify delivery for each connection and resource.
#[tokio::test]
async fn status_only_peer_recovery_excludes_private_cut_and_paging_handle() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({"model":"faux/echo", "plugins":{"permissions":{"enabled":false}}, "peer":{"allow":["baseline.read"]}}),
                load_init: false,
                project_trusted: false,
                args: json!({"no_session":true}),
                no_user_plugins: true,
            }).await.unwrap();
            seed_private_baseline(&host);
            let (mut cr, mut cw, task, sid) = open_test_conn(&host, false, 1 << 20).await;
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{
                "session_id":sid,"session_epoch":"old-epoch","last_seq":0,"include":["status/changed"]
            }})).await;
            let (reply, _) = read_reply_and_pre(&mut cr, 2).await;
            assert_eq!(reply["result"]["mode"], "baseline", "{reply}");
            let snapshot = rpc_read(&mut cr).await;
            assert_eq!(snapshot["method"], "snapshot", "{snapshot}");
            let raw = snapshot.to_string();
            assert!(!raw.contains("PRIVATE_QUEUE_TEXT") && !raw.contains("PRIVATE_ENTRY_TEXT"), "status-only snapshot leaked private content: {raw}");
            assert!(snapshot["params"].get("baseline_id").is_none(), "status-only peer must not receive a paging handle: {snapshot}");
            assert!(snapshot["params"]["parts"].as_array().is_some_and(Vec::is_empty), "{snapshot}");
            drop((cw, cr));
            task.await.unwrap();
        }).await;
}

async fn check_events_revocation(grant: &str, revoked_part: &str, read_before_revocation: bool) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({"model":"faux/echo", "plugins":{"permissions":{"enabled":false}}, "peer":{"allow":[grant,"baseline.read"]}}),
                load_init: false,
                project_trusted: false,
                args: json!({"no_session":true}),
                no_user_plugins: true,
            }).await.unwrap();
            seed_private_baseline(&host);
            let (mut cr, mut cw, task, sid) = open_test_conn(&host, false, 1 << 20).await;
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{
                "session_id":sid,"session_epoch":"old-epoch","last_seq":0
            }})).await;
            let (reply, _) = read_reply_and_pre(&mut cr, 2).await;
            assert_eq!(reply["result"]["mode"], "baseline", "{grant} must authorize subscribe: {reply}");
            let snapshot = rpc_read(&mut cr).await;
            assert!(snapshot.to_string().contains("PRIVATE_QUEUE_TEXT"), "full grant must retain queue: {snapshot}");
            let baseline_id = snapshot["params"]["baseline_id"].as_str().unwrap().to_owned();
            if read_before_revocation {
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"baseline.read","params":{
                    "baseline_id":baseline_id,"part":"entries"
                }})).await;
                let (page, _) = read_reply_and_pre(&mut cr, 3).await;
                assert!(page.to_string().contains("PRIVATE_ENTRY_TEXT"), "authorized page missing entry: {page}");
            }

            set_peer_allow(&host, &["baseline.read"]);
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"baseline.read","params":{
                "baseline_id":baseline_id,"part":revoked_part
            }})).await;
            let (revoked, _) = read_reply_and_pre(&mut cr, 4).await;
            assert!(!revoked.to_string().contains("PRIVATE_ENTRY_TEXT"), "revoked baseline leaked a page: {revoked}");
            assert!(revoked.get("error").is_some(), "{revoked}");
            set_peer_allow(&host, &[grant, "baseline.read"]);
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":5,"method":"baseline.read","params":{
                "baseline_id":baseline_id,"part":revoked_part
            }})).await;
            let (restored, _) = read_reply_and_pre(&mut cr, 5).await;
            assert_eq!(restored["error"]["data"]["code"], "baseline.stale", "old handle revived after regrant: {restored}");
            drop((cw, cr));
            task.await.unwrap();
        }).await;
}

#[tokio::test]
async fn events_grant_allows_subscribe_and_revocation_invalidates_baseline() {
    check_events_revocation("events.subscribe", "active_items", true).await;
}

#[tokio::test]
async fn revoking_events_invalidates_existing_baseline_even_when_read_is_granted() {
    check_events_revocation("events.subscribe", "entries", false).await;
}

#[tokio::test]
async fn revoking_events_blocks_already_queued_private_events_and_lag_snapshot() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "peer": { "allow": ["events.subscribe", "baseline.read"] }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            seed_private_baseline(&host);

            let (mut cr, mut cw, task, _sid) = open_test_conn(&host, false, 4 * 1024 * 1024).await;
            // Emit a non-status event (caches `Some(true)` in ConsumerSlot.decisions) then immediately revoke `events`
            // before the event pump runs.
            host.kernel
                .event_log()
                .admit_json("host/notice", json!({ "level": "info", "text": "QUEUED_BEFORE_REVOKE" }))
                .await
                .unwrap();
            set_peer_allow(&host, &[]);

            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 2, "method": "status.get", "params": {} })).await;
            let (status_reply, pre) = read_reply_and_pre(&mut cr, 2).await;
            assert!(status_reply.get("error").is_none(), "{status_reply}");
            let leaked = pre.iter().any(|m| m.to_string().contains("QUEUED_BEFORE_REVOKE"));
            assert!(!leaked, "revoked peer received pre-revoke queued event: {pre:?}");

            // Overflow the ring after revocation to force a `Lagged` re-anchor snapshot; it must exclude
            // private queue/entry state and withhold any `baseline_id`.
            for i in 0..=(mare_kernel::SESSION_LOG_CAPACITY + 64) {
                host.kernel
                    .event_log()
                    .admit_json("host/notice", json!({ "level": "info", "text": format!("lag-{i}") }))
                    .await
                    .unwrap();
            }
            tokio::task::yield_now().await;
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 3, "method": "status.get", "params": {} })).await;
            let (status2, pre2) = read_reply_and_pre(&mut cr, 3).await;
            assert!(status2.get("error").is_none(), "{status2}");
            let snap = pre2
                .iter()
                .find(|m| m["method"] == "snapshot")
                .unwrap_or_else(|| panic!("expected re-anchor snapshot after lag: {pre2:?}"));
            let raw_snap = snap.to_string();
            assert!(
                !raw_snap.contains("PRIVATE_QUEUE_TEXT") && !raw_snap.contains("PRIVATE_ENTRY_TEXT"),
                "lag re-anchor snapshot after revocation leaked private content: {raw_snap}"
            );
            assert!(
                snap["params"].get("baseline_id").is_none(),
                "revoked peer must not receive a paging handle on lag re-anchor: {snap}"
            );
            assert!(snap["params"]["parts"].as_array().is_some_and(Vec::is_empty), "{snap}");

            drop((cw, cr));
            task.await.unwrap();
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn baseline_is_stale_after_idle_expiry_with_injected_clock() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (mut cr, mut cw, server_task, session_id) = open_test_conn(&host, true, 1 << 20).await;

            // Trigger baseline cut.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"session_id":session_id}}),
            )
            .await;
            let (sub, _) = read_reply_and_pre(&mut cr, 2).await;
            assert_eq!(sub["result"]["mode"], "baseline", "{sub}");
            let snap = rpc_read(&mut cr).await;
            assert_eq!(snap["method"], "snapshot", "{snap}");
            let baseline_id = snap["params"]["baseline_id"].as_str().unwrap().to_string();

            // 59s elapsed -> still valid, reading `active_items` refreshes the idle timer.
            tokio::time::advance(Duration::from_secs(59)).await;
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"baseline.read","params":{"baseline_id":baseline_id,"part":"active_items"}}),
            )
            .await;
            let (r1, _) = read_reply_and_pre(&mut cr, 3).await;
            assert!(r1["result"].get("data").is_some(), "expected data at 59s: {r1}");

            // 61s more without reads -> idle expired -> stale.
            tokio::time::advance(Duration::from_secs(61)).await;
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"baseline.read","params":{"baseline_id":baseline_id,"part":"entries"}}),
            )
            .await;
            let (r2, _) = read_reply_and_pre(&mut cr, 4).await;
            assert_eq!(r2["error"]["data"]["code"], "baseline.stale", "{r2}");

            drop((cw, cr));
            server_task.await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn a_slow_peer_gets_lagged_then_a_new_baseline_without_disconnect_inproc() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "peer": { "allow": ["events*", "events.subscribe", "events.list", "baseline.read", "agent.prompt"] }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (mut cr, mut cw, server_task, session_id) = open_test_conn(&host, false, 4 * 1024 * 1024).await;
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"session_id":session_id}}),
            )
            .await;
            let (sub, _) = read_reply_and_pre(&mut cr, 2).await;
            assert!(sub.get("error").is_none(), "{sub}");
            let first = rpc_read(&mut cr).await;
            assert_eq!(first["method"], "snapshot", "{first}");

            // Overflow the SESSION_LOG_CAPACITY while the connection hasn't polled `drain_all`
            // (all admits run without yielding to the background `events` task).
            for i in 0..=(mare_kernel::SESSION_LOG_CAPACITY + 64) {
                host.kernel
                    .event_log()
                    .admit_json("host/notice", json!({"level": "info", "text": format!("msg-{i}")}))
                    .await
                    .unwrap();
            }
            tokio::task::yield_now().await;
            host.kernel
                .event_log()
                .admit_json("host/notice", json!({"level": "info", "text": "after-lag"}))
                .await
                .unwrap();

            let mut saw_lagged = false;
            let mut lag_identity = None;
            let mut saw_snapshot_after_lagged = false;
            let mut snap_seq = 0u64;
            let mut bid = String::new();
            let mut seen_after_lag_event = false;
            for _ in 0..20 {
                let m = tokio::time::timeout(Duration::from_secs(5), rpc_read(&mut cr))
                    .await
                    .expect("timed out waiting for lagged -> snapshot");
                if m["method"] == "lagged" {
                    assert_eq!(m["params"]["session_id"], first["params"]["session_id"], "{m}");
                    assert_eq!(m["params"]["session_epoch"], first["params"]["session_epoch"], "{m}");
                    lag_identity = Some((m["params"]["session_id"].clone(), m["params"]["session_epoch"].clone()));
                    saw_lagged = true;
                } else if saw_lagged && m["method"] == "snapshot" {
                    assert_eq!(
                        lag_identity.as_ref(),
                        Some(&(m["params"]["session_id"].clone(), m["params"]["session_epoch"].clone())),
                        "{m}"
                    );
                    saw_snapshot_after_lagged = true;
                    snap_seq = m["params"]["seq"].as_u64().unwrap();
                    bid = m["params"]["baseline_id"].as_str().unwrap().to_string();
                } else if saw_snapshot_after_lagged
                    && m["method"] == "event"
                    && m["params"]["seq"].as_u64().is_some_and(|s| s > snap_seq)
                {
                    assert_eq!(m["params"]["seq"].as_u64(), Some(snap_seq + 1), "{m}");
                    seen_after_lag_event = true;
                    break;
                }
            }
            assert!(saw_lagged, "expected lagged notification");
            assert!(saw_snapshot_after_lagged, "expected new snapshot immediately after lagged");
            assert!(seen_after_lag_event, "expected live event after recovery without disconnect");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"baseline.read","params":{"baseline_id":bid,"part":"entries"}}),
            )
            .await;
            let (res, _) = read_reply_and_pre(&mut cr, 3).await;
            assert!(res["result"].get("data").is_some(), "{res}");

            drop((cw, cr));
            server_task.await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn a_reconnecting_client_rebuilds_mid_stream_and_concurrent_writes() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "gated/stream",
                    "plugins": { "permissions": { "enabled": false } },
                    "peer": { "allow": ["events.subscribe", "events.list", "baseline.read"] }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            // Register a gated Lua provider via `llm:register_provider` that yields on a channel
            // mid-stream after emitting "alpha", then emits "beta" when released!
            let half_stream_sent = Arc::new(Notify::new());
            let release_stream = Arc::new(Notify::new());
            {
                let lua = host.kernel.lua();
                let sent = Arc::clone(&half_stream_sent);
                let rel = Arc::clone(&release_stream);
                let gate = lua
                    .create_async_function(move |_, ()| {
                        let sent = Arc::clone(&sent);
                        let rel = Arc::clone(&rel);
                        async move {
                            sent.notify_one();
                            rel.notified().await;
                            Ok(())
                        }
                    })
                    .unwrap();
                lua.globals().set("__test_stream_gate", gate).unwrap();
                let reg_fn: mlua::Function = lua
                    .load(
                        r#"
                        local llm = ...
                        llm:register_provider({
                            provider = "gated",
                            default_model = "stream",
                            is_configured = function() return true end,
                            models = { { model_id = "stream", name = "Gated Stream", context_window = 100000, max_output = 4096 } },
                            stream = function(req, sink)
                                sink({ type = "text_delta", delta = "alpha" })
                                -- Switching delta kinds flushes the prior coalesced delta onto ctx.bus synchronously.
                                sink({ type = "reasoning_delta", delta = "pause" })
                                __test_stream_gate()
                                sink({ type = "text_delta", delta = "beta" })
                                return {
                                    role = "assistant",
                                    content = { { type = "text", text = "alphabeta" } },
                                    stop_reason = "stop",
                                }
                            end,
                        })
                        "#,
                    )
                    .into_function()
                    .unwrap();
                let llm_tbl = host.kernel.service("llm").unwrap();
                reg_fn.call::<()>(llm_tbl).unwrap();
            }

            // Connect Live Client 1 (primary).
            let (mut cr1, mut cw1, st1, sid) = open_test_conn(&host, true, 1 << 20).await;
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{}})).await;
            let _ = read_reply_and_pre(&mut cr1, 2).await;

            // Start a prompt that pauses mid-stream after emitting "alpha".
            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":3,"method":"agent.prompt","params":{"session_id":sid,"input":"hi"}}),
            )
            .await;
            half_stream_sent.notified().await;

            // Drain Live Client 1 events until we observe the "alpha" item/delta notification!
            let mut live_text = String::new();
            let mut pre_cut_events = Vec::new();
            loop {
                let m = rpc_read(&mut cr1).await;
                if m.get("id").and_then(J::as_u64) == Some(3) && m.get("error").is_some() {
                    panic!("prompt failed unexpectedly: {m}");
                }
                if m["method"] == "event" {
                    if m["params"]["type"] == "item/delta"
                        && m["params"]["data"]["kind"] == "text"
                    {
                        live_text.push_str(m["params"]["data"]["delta"].as_str().unwrap());
                    }
                    pre_cut_events.push(m);
                    if live_text.contains("alpha") {
                        break;
                    }
                }
            }
            assert_eq!(live_text, "alpha");

            // Reconnecting Client 2 subscribes MID-STREAM!
            let (c2, s2) = tokio::io::duplex(1 << 20);
            let (sr2, sw2) = tokio::io::split(s2);
            let st2 = tokio::task::spawn_local(serve(host.clone(), sr2, sw2, false, std::future::pending()));
            let (cr2, mut cw2) = tokio::io::split(c2);
            let mut cr2 = BufReader::new(cr2);
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}})).await;
            let (init2, _) = read_reply_and_pre(&mut cr2, 1).await;
            let session_id = init2["result"]["session"]["session_id"]
                .as_str()
                .unwrap_or_else(|| panic!("missing session id in {init2}"))
                .to_string();
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"session_id":session_id}}),
            )
            .await;
            let (sub2, pre2) = read_reply_and_pre(&mut cr2, 2).await;
            assert!(pre2.is_empty(), "{pre2:?}");
            assert_eq!(sub2["result"]["mode"], "baseline", "{sub2}");
            let snap2 = rpc_read(&mut cr2).await;
            assert_eq!(snap2["method"], "snapshot", "{snap2}");
            let snap_seq = snap2["params"]["seq"].as_u64().unwrap();
            assert_eq!(sub2["result"]["seq"].as_u64(), Some(snap_seq + 1), "{sub2} vs {snap2}");
            let bid = snap2["params"]["baseline_id"].as_str().unwrap().to_string();

            // Retained skeleton must reflect active_turn + active_items with offset = 5 ("alpha")!
            assert_eq!(snap2["params"]["retained"]["state"], "running", "{snap2}");
            let skel = &snap2["params"]["retained"]["active_items"];
            assert_eq!(skel.as_array().unwrap().len(), 1, "{snap2}");
            assert_eq!(skel[0]["content"][0]["offset"], 5, "{snap2}");

            // NOW release the second half of the stream ("beta" + turn completion) WHILE Client 2 pages!
            release_stream.notify_one();
            loop {
                let m = rpc_read(&mut cr1).await;
                if m["method"] == "event" {
                    if m["params"]["type"] == "item/delta"
                        && m["params"]["data"]["kind"] == "text"
                    {
                        live_text.push_str(m["params"]["data"]["delta"].as_str().unwrap());
                    }
                    if m["params"]["type"] == "turn/completed" {
                        break;
                    }
                }
            }
            assert_eq!(live_text, "alphabeta");

            // Client 2 reads BOTH baseline parts AFTER turn completion and buffers post-S events.
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":3,"method":"baseline.read","params":{"baseline_id":bid,"part":"active_items"}}),
            )
            .await;
            let (ai_res, mut buf2) = read_reply_and_pre(&mut cr2, 3).await;
            let ai_data = ai_res["result"]["data"].as_array().unwrap();
            assert_eq!(ai_data.len(), 1, "{ai_res}");
            assert_eq!(ai_data[0]["content"][0]["text"], "alpha");
            assert_eq!(ai_data[0]["content"][0]["offset"], 5);
            let mut rc_text = ai_data[0]["content"][0]["text"].as_str().unwrap().to_string();

            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":4,"method":"baseline.read","params":{"baseline_id":bid,"part":"entries"}}),
            )
            .await;
            let (ent_res, more) = read_reply_and_pre(&mut cr2, 4).await;
            buf2.extend(more);
            let ent_data = ent_res["result"]["data"].as_array().unwrap();
            // User entry was committed pre-stream; assistant message was NOT in `entries` at cut S.
            assert!(ent_data.iter().any(|e| e["type"] == "message" && e["message"]["role"] == "user"));
            assert!(!ent_data.iter().any(|e| e["type"] == "message" && e["message"]["role"] == "assistant"));

            while !buf2.iter().any(|v| v["method"] == "event" && v["params"]["type"] == "turn/completed") {
                buf2.push(rpc_read(&mut cr2).await);
            }
            let evs: Vec<&J> = buf2.iter().filter(|v| v["method"] == "event").collect();
            assert_eq!(evs.first().unwrap()["params"]["seq"].as_u64(), Some(snap_seq + 1));
            for w in evs.windows(2) {
                assert_eq!(w[1]["params"]["seq"].as_u64(), Some(w[0]["params"]["seq"].as_u64().unwrap() + 1));
            }
            for ev in evs {
                if ev["params"]["type"] == "item/delta" && ev["params"]["data"]["kind"] == "text" {
                    rc_text.push_str(ev["params"]["data"]["delta"].as_str().unwrap());
                }
            }
            assert_eq!(rc_text, live_text, "reconnecting client must reconstruct exact same prefix/text as live client");

            drop((cw2, cr2));
            st2.await.unwrap();
            drop((cw1, cr1));
            st1.await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn baseline_supersede_disconnect_release_and_wire_projection() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "peer": { "allow": ["events*", "events.subscribe", "events.list", "baseline.read", "agent.prompt", "session.create"] }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            // Register a runtime secret AFTER a queue item / entry exists to prove egress projection on snapshot & baseline.read,
            // and instrument `session.baseline_cut` release hooks to verify supersede + close release.
            {
                let lua = host.kernel.lua();
                let fn_setup: mlua::Function = lua
                    .load(
                        r#"
                        local s, ag = ...
                        ag:set_queued({ steer = { { input_id = "q1", text = "check SUPA_RUNTIME_SECRET_42", images = { { mime = "image/png", data = "BASE64_SHOULD_NOT_BE_IN_CUT", size = 123, path = "/tmp/p.png" } } } } })
                        s:persist_message({ message = { role = "user", content = { { type = "text", text = "entry SUPA_RUNTIME_SECRET_42" } } } })
                        _G.__sec_lease = mare.projection.register("SUPA_RUNTIME_SECRET_42")
                        _G.__sec_ref = _G.__sec_lease.reference
                        _G.__baseline_release_calls = 0
                        local orig_cut = s.baseline_cut
                        function s.baseline_cut(...)
                            local res = orig_cut(...)
                            if type(res) == "table" and type(res.parts) == "table" then
                                for _, p in ipairs(res.parts) do
                                    local orig_rel = p.release
                                    p.release = function(...)
                                        _G.__baseline_release_calls = _G.__baseline_release_calls + 1
                                        if orig_rel then return orig_rel(...) end
                                    end
                                end
                            end
                            return res
                        end
                        "#,
                    )
                    .into_function()
                    .unwrap();
                fn_setup
                    .call::<()>((host.kernel.service("session").unwrap(), host.kernel.service("agent").unwrap()))
                    .unwrap();
            }

            let (mut cr1, mut cw1, st1, _s1) = open_test_conn(&host, true, 1 << 20).await;
            let (mut cr2, mut cw2, st2, s1_id) = open_test_conn(&host, false, 1 << 20).await;

            // Subscribe peer 2 twice on the same target to supersede bid1; verify snapshot redacts & strips image base64.
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"session_id":s1_id}})).await;
            let expected_ref: String = host.kernel.lua().load("return _G.__sec_ref").eval().unwrap();
            let (sub1, _) = read_reply_and_pre(&mut cr2, 2).await;
            assert_eq!(sub1["result"]["mode"], "baseline", "{sub1}");
            let snap1 = rpc_read(&mut cr2).await;
            let snap1_str = snap1.to_string();
            assert!(!snap1_str.contains("SUPA_RUNTIME_SECRET_42"), "snapshot leaked secret: {snap1_str}");
            assert!(snap1_str.contains(&expected_ref), "snapshot missing redacted token {expected_ref}: {snap1_str}");
            assert!(!snap1_str.contains("BASE64_SHOULD_NOT_BE_IN_CUT"), "snapshot queue leaked raw base64: {snap1_str}");
            let bid1 = snap1["params"]["baseline_id"].as_str().unwrap().to_string();

            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":3,"method":"events.subscribe","params":{"session_id":s1_id}})).await;
            let _ = read_reply_and_pre(&mut cr2, 3).await;
            let snap2 = rpc_read(&mut cr2).await;
            let bid2 = snap2["params"]["baseline_id"].as_str().unwrap().to_string();
            let rel_after_supersede: i64 = host.kernel.lua().load("return _G.__baseline_release_calls").eval().unwrap();
            assert_eq!(rel_after_supersede, 1, "superseding bid1 must call release hook");

            // Superseded bid1 must return baseline.stale.
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":4,"method":"baseline.read","params":{"baseline_id":bid1,"part":"entries"}})).await;
            let (r_stale, _) = read_reply_and_pre(&mut cr2, 4).await;
            assert_eq!(r_stale["error"]["data"]["code"], "baseline.stale", "{r_stale}");

            // Invalid cursor must return coded `invalid_arguments`, and leave `entries` readable.
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":5,"method":"baseline.read","params":{"baseline_id":bid2,"part":"entries","cursor":"bad"}})).await;
            let (r_bad_cur, _) = read_reply_and_pre(&mut cr2, 5).await;
            assert_eq!(r_bad_cur["error"]["data"]["code"], "invalid_arguments", "{r_bad_cur}");

            // Reading `entries` on bid2 must redact the runtime secret at the RPC export boundary (leaving `active_items` unread).
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":6,"method":"baseline.read","params":{"baseline_id":bid2,"part":"entries"}})).await;
            let (r_entries, _) = read_reply_and_pre(&mut cr2, 6).await;
            let r_entries_str = r_entries.to_string();
            assert!(!r_entries_str.contains("SUPA_RUNTIME_SECRET_42"), "baseline.read leaked runtime secret: {r_entries_str}");
            assert!(r_entries_str.contains(&expected_ref), "baseline.read missing redacted token {expected_ref}: {r_entries_str}");

            // Unknown part while `active_items` is still unread must return coded `baseline.unknown_part`, not internal -32000.
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":7,"method":"baseline.read","params":{"baseline_id":bid2,"part":"no_such_part"}})).await;
            let (r_bad_part, _) = read_reply_and_pre(&mut cr2, 7).await;
            assert_eq!(r_bad_part["error"]["data"]["code"], "baseline.unknown_part", "{r_bad_part}");

            // Disconnecting peer 2 releases remaining unread parts of bid2.
            drop((cw2, cr2));
            st2.await.unwrap();
            let rel_after_close: i64 = host.kernel.lua().load("return _G.__baseline_release_calls").eval().unwrap();
            assert_eq!(rel_after_close, 2, "disconnecting peer must release unread baseline parts");

            // session.create works cleanly for peer after switch.
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{}})).await;
            let (sn, _) = read_reply_and_pre(&mut cr1, 2).await;
            assert!(sn.get("error").is_none(), "{sn}");

            drop((cw1, cr1));
            st1.await.unwrap();
        })
        .await;
}

/// A final page that fails strict encoding must leave its baseline available for retry.
#[tokio::test]
async fn oversized_final_baseline_page_does_not_consume_the_handle() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({ "model": "faux/echo", "plugins": { "permissions": { "enabled": false } } }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            let setup: mlua::Function = host
                .kernel
                .lua()
                .load(
                    r#"local s = ...
                       _G.__page_mode, _G.__page_releases = 'oversized', 0
                       local original = s.baseline_cut
                       function s.baseline_cut(...)
                         local cut = original(...)
                         for _, part in ipairs(cut.parts or {}) do
                           if part.name == 'entries' then
                             local release = part.release
                             part.release = function()
                               _G.__page_releases = _G.__page_releases + 1
                               if release then release() end
                             end
                             part.page = function()
                               if _G.__page_mode == 'oversized' then
                                 local entries = mare.json.array({})
                                 for i = 1, 10050 do entries[i] = i end
                                 return { data = mare.json.array({ { nested = entries } }) }
                               end
                               if _G.__page_mode == 'invalid' then return { data = 'not-an-array' } end
                               return { data = mare.json.array({ { text = 'recovered' } }) }
                             end
                           end
                         end
                         return cut
                       end"#,
                )
                .into_function()
                .unwrap();
            setup.call::<()>(host.kernel.service("session").unwrap()).unwrap();
            let (mut cr, mut cw, server, sid) = open_test_conn(&host, true, 4 * 1024 * 1024).await;
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"session_id":sid}}))
                .await;
            let _ = read_reply_and_pre(&mut cr, 2).await;
            let snapshot = rpc_read(&mut cr).await;
            let bid = snapshot["params"]["baseline_id"].as_str().unwrap();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"baseline.read","params":{"baseline_id":bid,"part":"entries"}}),
            )
            .await;
            let (first, _) = read_reply_and_pre(&mut cr, 3).await;
            let releases: i64 = host.kernel.lua().load("return _G.__page_releases").eval().unwrap();
            assert_eq!(releases, 0, "failed encoding must not release the final page");
            host.kernel.lua().load("_G.__page_mode = 'invalid'").exec().unwrap();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"baseline.read","params":{"baseline_id":bid,"part":"entries"}}),
            )
            .await;
            let (invalid, _) = read_reply_and_pre(&mut cr, 4).await;
            assert_eq!(
                [first["error"]["data"]["code"].as_str(), invalid["error"]["data"]["code"].as_str()],
                [Some("invalid_result"), Some("invalid_result")],
                "oversized={first}; invalid={invalid}"
            );
            let releases: i64 = host.kernel.lua().load("return _G.__page_releases").eval().unwrap();
            assert_eq!(releases, 0, "failed contract validation must not release the final page");
            host.kernel.lua().load("_G.__page_mode = 'valid'").exec().unwrap();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"baseline.read","params":{"baseline_id":bid,"part":"entries"}}),
            )
            .await;
            let (retry, _) = read_reply_and_pre(&mut cr, 5).await;
            assert_eq!(retry["result"]["data"][0]["text"], "recovered", "{retry}");
            let releases: i64 = host.kernel.lua().load("return _G.__page_releases").eval().unwrap();
            assert_eq!(releases, 1);
            drop((cw, cr));
            server.await.unwrap();
        })
        .await;
}

/// A single oversized history item fails repeatedly instead of consuming the last page.
#[tokio::test]
async fn baseline_single_oversized_item_is_recoverable() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({ "model": "faux/echo", "plugins": { "permissions": { "enabled": false } } }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            let add: mlua::Function = host.kernel.lua().load(
                r#"local session = ...
                   local medium = mare.json.array({})
                   for i = 1, 2600 do medium[i] = { type = 'text', text = 'medium-' .. i } end
                   session:persist_message({ message = { role = 'user', content = medium } })
                   local content = mare.json.array({})
                   for i = 1, 4000 do content[i] = { type = 'text', text = 'entry-' .. i } end
                   session:persist_message({ message = { role = 'user', content = content } })"#,
            ).into_function().unwrap();
            add.call::<()>(host.kernel.service("session").unwrap()).unwrap();
            let (mut cr, mut cw, server, sid) = open_test_conn(&host, true, 4 * 1024 * 1024).await;
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"session_id":sid}})).await;
            let _ = read_reply_and_pre(&mut cr, 2).await;
            let snapshot = rpc_read(&mut cr).await;
            let bid = snapshot["params"]["baseline_id"].as_str().unwrap();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"baseline.read","params":{"baseline_id":bid,"part":"entries"}})).await;
            let (prefix, _) = read_reply_and_pre(&mut cr, 3).await;
            let cursor = prefix["result"]["next_cursor"].as_str().expect("a small model record precedes the large items");
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"baseline.read","params":{"baseline_id":bid,"part":"entries","cursor":cursor}})).await;
            let (medium, _) = read_reply_and_pre(&mut cr, 4).await;
            assert!(medium.get("error").is_none(), "an encodable item must not hit the soft paging budget: {medium}");
            assert_eq!(medium["result"]["data"][0]["message"]["content"].as_array().unwrap().len(), 2600, "a single encodable item above the soft paging budget must be delivered: {medium}");
            let cursor = medium["result"]["next_cursor"].as_str().expect("the oversized item follows");
            for id in [5, 6] {
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":id,"method":"baseline.read","params":{"baseline_id":bid,"part":"entries","cursor":cursor}})).await;
                let (reply, _) = read_reply_and_pre(&mut cr, id).await;
                assert_eq!(reply["error"]["data"]["code"], "baseline.item_too_large", "{reply}");
            }
            drop((cw, cr));
            server.await.unwrap();
        })
        .await;
}

/// Regression: ~800 short `message` entries totaling well under 512 KiB must still page
/// cleanly below `strict_lua_to_json`'s 10,000-node limit without failing page 1.
#[tokio::test]
async fn baseline_pages_800_short_message_entries_below_strict_json_node_limit() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            {
                let lua = host.kernel.lua();
                let session = host.kernel.service("session").unwrap();
                let add_fn: mlua::Function = lua
                    .load(
                        r#"
                        local s = ...
                        for i = 1, 800 do
                            s:persist_message({
                                message = {
                                    role = (i % 2 == 1) and "user" or "assistant",
                                    content = { { type = "text", text = "m-" .. i } },
                                    usage = { input = 1, output = 1, cost = 0 },
                                },
                            })
                        end
                        "#,
                    )
                    .into_function()
                    .unwrap();
                add_fn.call::<mlua::Value>(session).unwrap();
            }

            let (mut cr, mut cw, st, sid) = open_test_conn(&host, true, 4 * 1024 * 1024).await;
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"session_id":sid}}))
                .await;
            let _ = read_reply_and_pre(&mut cr, 2).await;
            let snap = rpc_read(&mut cr).await;
            assert_eq!(snap["method"], "snapshot", "{snap}");
            let bid = snap["params"]["baseline_id"].as_str().unwrap().to_string();

            let mut pages = 0usize;
            let mut total_items = 0usize;
            let mut cursor: Option<String> = None;
            let mut id = 10u64;
            loop {
                let mut params = json!({ "baseline_id": bid, "part": "entries" });
                if let Some(c) = cursor.take() {
                    params["cursor"] = json!(c);
                }
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":id,"method":"baseline.read","params":params})).await;
                let (reply, _) = read_reply_and_pre(&mut cr, id).await;
                id += 1;
                assert!(reply.get("error").is_none(), "page {pages} must not hit JSON node limit: {reply}");
                let arr = reply["result"]["data"].as_array().unwrap();
                total_items += arr.len();
                pages += 1;
                match reply["result"].get("next_cursor").and_then(J::as_str) {
                    Some(n) => cursor = Some(n.to_string()),
                    None => break,
                }
            }
            assert!(pages >= 2, "800 message entries (~15+ nodes each) must split into >= 2 pages, got {pages}");
            assert!(total_items >= 800, "expected all >= 800 entries, got {total_items}");

            drop((cw, cr));
            st.await.unwrap();
        })
        .await;
}

/// Dual-runtime isolation regression: when a separate runtime (`s2`) lags/overflows its ring,
/// an old-session cursor on `s1` never sees `s2` snapshots, `lagged`, or private queue state.
#[tokio::test]
async fn dual_runtime_lagged_old_session_cursor_does_not_receive_other_runtime_state() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            struct EnvRestore(Option<std::ffi::OsString>);
            impl Drop for EnvRestore {
                fn drop(&mut self) {
                    match &self.0 {
                        Some(value) => unsafe { std::env::set_var("MARE_HOME", value) },
                        None => unsafe { std::env::remove_var("MARE_HOME") },
                    }
                }
            }
            let _env_restore = EnvRestore(std::env::var_os("MARE_HOME"));
            unsafe { std::env::set_var("MARE_HOME", dir.path().join("mare-home")) };

            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "peer": { "allow": ["events.subscribe", "events.list", "baseline.read"] }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({}),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let s1 = manager.create_session(opts.clone()).await.unwrap();
            let s1_id = s1.session_id.clone();
            let rpc = RpcRuntime::new(manager.clone(), opts, s1.host.clone(), Some(s1_id.clone()), false);

            let (c1, srv1) = tokio::io::duplex(1 << 20);
            let (sr1, sw1) = tokio::io::split(srv1);
            let st1 = tokio::task::spawn_local(serve_manager(
                rpc.clone(),
                sr1,
                sw1,
                true,
                None,
                std::future::pending(),
            ));
            let (cr1, mut cw1) = tokio::io::split(c1);
            let mut cr1 = BufReader::new(cr1);
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}})).await;
            let (init1, _) = read_reply_and_pre(&mut cr1, 1).await;
            assert!(init1.get("error").is_none(), "{init1}");

            let (c2, srv2) = tokio::io::duplex(4 * 1024 * 1024);
            let (sr2, sw2) = tokio::io::split(srv2);
            let st2 = tokio::task::spawn_local(serve_manager(
                rpc.clone(),
                sr2,
                sw2,
                false,
                None,
                std::future::pending(),
            ));
            let (cr2, mut cw2) = tokio::io::split(c2);
            let mut cr2 = BufReader::new(cr2);
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}})).await;
            let (init2, _) = read_reply_and_pre(&mut cr2, 1).await;
            assert!(init2.get("error").is_none(), "{init2}");

            // Peer 2 subscribes explicitly to s1.
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"session_id":s1_id}}),
            )
            .await;
            let (sub, _) = read_reply_and_pre(&mut cr2, 2).await;
            assert_eq!(sub["result"]["mode"], "baseline");
            let init_snap = rpc_read(&mut cr2).await;
            assert_eq!(init_snap["params"]["session_id"], s1_id);

            // Create a separate second runtime (`s2`) via RPC, set private queued state, and overflow s2's ring.
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{}})).await;
            let (sn, _) = read_reply_and_pre(&mut cr1, 2).await;
            assert!(sn.get("error").is_none(), "{sn}");
            let s2_id = sn["result"]["session_id"].as_str().unwrap().to_owned();
            assert_ne!(s2_id, s1_id);
            let s2 = manager.session(&s2_id).unwrap();
            {
                let lua = s2.host.kernel.lua();
                let fn_q: mlua::Function = lua
                    .load(r#"local ag = ...; ag:set_queued({ steer = { { input_id = "q2", text = "S2_PRIVATE_STEER", images = {} } } })"#)
                    .into_function()
                    .unwrap();
                fn_q.call_async::<()>(s2.host.kernel.service("agent").unwrap()).await.unwrap();
            }

            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let b1 = Arc::clone(&barrier);
            let b2 = Arc::clone(&barrier);
            let s1_host = Arc::clone(&s1.host);
            let s2_host = Arc::clone(&s2.host);
            let emit_s2 = tokio::task::spawn_local(async move {
                b2.wait().await;
                for i in 0..=(mare_kernel::SESSION_LOG_CAPACITY + 64) {
                    s2_host
                        .kernel
                        .event_log()
                        .admit_json("host/notice", json!({"level":"info","text": format!("s2-{i}")}))
                        .await
                        .unwrap();
                }
            });
            let emit_s1 = tokio::task::spawn_local(async move {
                b1.wait().await;
                s1_host
                    .kernel
                    .event_log()
                    .admit_json("host/notice", json!({"level":"info","text":"s1-isolated-check"}))
                    .await
                    .unwrap();
            });
            let (r1, r2) = tokio::join!(emit_s1, emit_s2);
            r1.unwrap();
            r2.unwrap();

            // Emit a trailing barrier marker on s1 after s2 finished overflowing; peer 2 must see s1-barrier-done
            // without ever observing S2_PRIVATE_STEER or any s2 event.
            s1.host
                .kernel
                .event_log()
                .admit_json("host/notice", json!({"level":"info","text":"s1-barrier-done"}))
                .await
                .unwrap();

            let mut saw_barrier = false;
            while let Ok(msg) = tokio::time::timeout(Duration::from_secs(5), rpc_read(&mut cr2)).await {
                let raw = msg.to_string();
                assert!(!raw.contains("S2_PRIVATE_STEER"), "s2 queue leaked into s1 subscriber: {raw}");
                assert_ne!(msg["params"]["session_id"], s2_id, "s2 event leaked into s1 subscriber: {raw}");
                if msg["method"] == "event" && msg["params"]["data"]["text"] == "s1-barrier-done" {
                    saw_barrier = true;
                    break;
                }
            }
            assert!(saw_barrier, "s1 subscriber must observe s1 barrier without interference from s2");

            drop((cw2, cr2));
            st2.await.unwrap();
            drop((cw1, cr1));
            st1.await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn same_host_session_switch_preserves_tail_and_continues_into_new_epoch() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            struct EnvRestore(Option<std::ffi::OsString>);
            impl Drop for EnvRestore {
                fn drop(&mut self) {
                    match &self.0 {
                        Some(value) => unsafe { std::env::set_var("MARE_HOME", value) },
                        None => unsafe { std::env::remove_var("MARE_HOME") },
                    }
                }
            }
            let _env_restore = EnvRestore(std::env::var_os("MARE_HOME"));
            unsafe { std::env::set_var("MARE_HOME", dir.path().join("mare-home")) };
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "peer": { "allow": ["events.subscribe", "events.list", "baseline.read"] }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (mut cr1, mut cw1, st1, s1_id) = open_test_conn(&host, true, 1 << 20).await;
            let (mut cr2, mut cw2, st2, _) = open_test_conn(&host, false, 1 << 20).await;

            // Peer 2 subscribes while on s1 (with s1_id specified) and consumes initial baseline.
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"session_id":s1_id}}),
            )
            .await;
            let (sub, _) = read_reply_and_pre(&mut cr2, 2).await;
            assert_eq!(sub["result"]["mode"], "baseline");
            let init_snap = rpc_read(&mut cr2).await;
            let s1_epoch = init_snap["params"]["session_epoch"].as_str().unwrap().to_owned();
            let s1_baseline_id = init_snap["params"]["baseline_id"].as_str().unwrap().to_owned();

            // Emit an unread s1 tail event, then switch this single Host to a new session s2.
            host.kernel
                .event_log()
                .admit_json("host/notice", json!({"level":"info","text":"s1-tail-before-switch"}))
                .await
                .unwrap();
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{}})).await;
            let (sn, _) = read_reply_and_pre(&mut cr1, 2).await;
            assert!(sn.get("error").is_none(), "{sn}");
            let s2_id = sn["result"]["session_id"].as_str().unwrap().to_owned();
            assert_ne!(s2_id, s1_id);
            let (_, s2_epoch) = host.kernel.event_log().session_identity();
            assert_ne!(s2_epoch, s1_epoch);

            // Reading the old s1 baseline after epoch rotation must return baseline.stale.
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":3,"method":"baseline.read","params":{"baseline_id":s1_baseline_id,"part":"entries"}}),
            )
            .await;
            let (stale_rep, pre) = read_reply_and_pre(&mut cr2, 3).await;
            assert_eq!(
                stale_rep["error"]["data"]["code"], "baseline.stale",
                "e1 baseline must be stale after switch: {stale_rep}"
            );

            host.kernel
                .event_log()
                .admit_json("host/notice", json!({"level":"info","text":"s2-after-switch"}))
                .await
                .unwrap();

            let mut seen_texts = Vec::new();
            let mut seen_epochs = Vec::new();
            for msg in pre {
                if msg["method"] == "event"
                    && let Some(text) = msg["params"]["data"]["text"].as_str()
                {
                    seen_texts.push(text.to_owned());
                    seen_epochs.push(msg["params"]["session_epoch"].as_str().unwrap().to_owned());
                }
            }
            while let Ok(msg) = tokio::time::timeout(Duration::from_secs(2), rpc_read(&mut cr2)).await {
                if msg["method"] == "event"
                    && let Some(text) = msg["params"]["data"]["text"].as_str()
                {
                    seen_texts.push(text.to_owned());
                    seen_epochs.push(msg["params"]["session_epoch"].as_str().unwrap().to_owned());
                    if text == "s2-after-switch" {
                        break;
                    }
                }
            }
            assert_eq!(
                seen_texts,
                vec!["s1-tail-before-switch".to_owned(), "s2-after-switch".to_owned()],
                "same-host switch must preserve s1 tail before delivering s2 events"
            );
            assert_eq!(seen_epochs, vec![s1_epoch, s2_epoch]);

            // Re-subscribing on the same Host after session switch (explicit session_id on peer, and
            // explicit s2 followed by parameterless filter_only subscribe on owner) must replace /
            // deduplicate prior slots on that Host so events are delivered exactly once (never 2x/Nx).
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":4,"method":"events.subscribe","params":{"session_id":s2_id}}),
            )
            .await;
            let (resub_peer, _) = read_reply_and_pre(&mut cr2, 4).await;
            assert!(resub_peer.get("error").is_none(), "{resub_peer}");
            if resub_peer["result"]["mode"] == "baseline" {
                let snap2 = rpc_read(&mut cr2).await;
                assert_eq!(snap2["method"], "snapshot", "{snap2}");
            }

            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":3,"method":"events.subscribe","params":{"session_id":s2_id}}),
            )
            .await;
            let (resub_owner_s2, _) = read_reply_and_pre(&mut cr1, 3).await;
            assert!(resub_owner_s2.get("error").is_none(), "{resub_owner_s2}");
            if resub_owner_s2["result"]["mode"] == "baseline" {
                let snap_owner = rpc_read(&mut cr1).await;
                assert_eq!(snap_owner["method"], "snapshot", "{snap_owner}");
            }

            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":4,"method":"events.subscribe","params":{"scope":"all"}}))
                .await;
            let (resub_owner_all, _) = read_reply_and_pre(&mut cr1, 4).await;
            assert!(resub_owner_all.get("error").is_none(), "{resub_owner_all}");

            host.kernel
                .event_log()
                .admit_json("host/notice", json!({"level":"info","text":"s2-dedup-target"}))
                .await
                .unwrap();

            async fn count_target_before_barrier(
                reader: &mut BufReader<ReadHalf<tokio::io::DuplexStream>>,
                writer: &mut WriteHalf<tokio::io::DuplexStream>,
                req_id: u64,
            ) -> usize {
                rpc_send(writer, json!({"jsonrpc":"2.0","id":req_id,"method":"events.list","params":{}})).await;
                let (rep, pre) = read_reply_and_pre(reader, req_id).await;
                assert!(rep.get("error").is_none(), "{rep}");
                pre.into_iter()
                    .filter(|msg| {
                        msg["method"] == "event" && msg["params"]["data"]["text"].as_str() == Some("s2-dedup-target")
                    })
                    .count()
            }
            assert_eq!(
                count_target_before_barrier(&mut cr2, &mut cw2, 5).await,
                1,
                "explicit re-subscribe on same Host must deliver each event exactly once"
            );
            assert_eq!(
                count_target_before_barrier(&mut cr1, &mut cw1, 5).await,
                1,
                "owner filter_only subscribe after same-Host switch must not duplicate cursors"
            );

            drop((cw2, cr2));
            st2.await.unwrap();
            drop((cw1, cr1));
            st1.await.unwrap();
        })
        .await;
}

/// Should-Fix #6: active `tool_call_delta` + running `agent/tool-start` + main-connection-only
/// `pending_approvals` recovery.
#[tokio::test]
async fn baseline_reconstructs_tool_call_delta_tool_execution_and_primary_approvals() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "toolgate/stream",
                    "plugins": { "permissions": { "enabled": false } },
                    "peer": { "allow": ["events*", "events.subscribe", "events.list", "baseline.read", "agent.prompt"] }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let tool_mid = Arc::new(Notify::new());
            let release_tool = Arc::new(Notify::new());
            {
                let lua = host.kernel.lua();
                let tm = Arc::clone(&tool_mid);
                let rt = Arc::clone(&release_tool);
                let gate = lua
                    .create_async_function(move |_, ()| {
                        let tm = Arc::clone(&tm);
                        let rt = Arc::clone(&rt);
                        async move {
                            tm.notify_one();
                            rt.notified().await;
                            Ok(())
                        }
                    })
                    .unwrap();
                lua.globals().set("__tool_exec_gate", gate).unwrap();
                let reg_fn: mlua::Function = lua
                    .load(
                        r#"
                        local llm = ...
                        llm:register_provider({
                            provider = "toolgate",
                            default_model = "stream",
                            is_configured = function() return true end,
                            models = { { model_id = "stream", name = "Tool Gate", context_window = 100000, max_output = 4096 } },
                            stream = function(req, sink)
                                sink({ type = "tool_call_start", tool_call_id = "call_tc_1", name = "slow_echo" })
                                sink({ type = "tool_call_delta", tool_call_id = "call_tc_1", delta = '{"msg":' })
                                sink({ type = "reasoning_delta", delta = "flush" })
                                __tool_exec_gate()
                                sink({ type = "tool_call_delta", tool_call_id = "call_tc_1", delta = '"hi"}' })
                                return { role = "assistant", content = { { type = "text", text = "done" } }, stop_reason = "stop" }
                            end,
                        })
                        "#,
                    )
                    .into_function()
                    .unwrap();
                reg_fn.call::<()>(host.kernel.service("llm").unwrap()).unwrap();
            }

            let (mut cr1, mut cw1, st1, sid) = open_test_conn(&host, true, 1 << 20).await;
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{}})).await;
            let _ = read_reply_and_pre(&mut cr1, 2).await;

            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":3,"method":"agent.prompt","params":{"session_id":sid,"input":"run tool"}}),
            )
            .await;
            let (pr_start, _) = read_reply_and_pre(&mut cr1, 3).await;
            assert!(pr_start.get("error").is_none(), "{pr_start}");
            let tid = pr_start["result"]["turn_id"].as_str().unwrap().to_string();
            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":4,"method":"agent.wait","params":{"session_id":sid,"turn_id":tid}}),
            )
            .await;
            tool_mid.notified().await;
            loop {
                let m = rpc_read(&mut cr1).await;
                if m["method"] == "event"
                    && m["params"]["type"] == "item/delta"
                    && m["params"]["data"]["kind"] == "tool_arguments"
                {
                    break;
                }
            }

            // Reconnecting peer gets active_items skeleton with exact offset = 7 bytes for `{"msg":`.
            let (mut cr2, mut cw2, st2, _) = open_test_conn(&host, false, 1 << 20).await;
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"session_id":sid}})).await;
            let (sub2, _) = read_reply_and_pre(&mut cr2, 2).await;
            assert_eq!(sub2["result"]["mode"], "baseline", "{sub2}");
            let snap2 = rpc_read(&mut cr2).await;
            assert_eq!(
                snap2["params"]["retained"]["pending_approvals"],
                json!([]),
                "peer must not see pending_approvals"
            );
            let skels = snap2["params"]["retained"]["active_items"].as_array().unwrap();
            let tc_skel = skels
                .iter()
                .flat_map(|item| item["content"].as_array().into_iter().flatten())
                .find(|b| b["id"] == "call_tc_1")
                .expect("tool_call skeleton");
            assert_eq!(tc_skel["offset"], 7, "tool_call delta offset must equal 7 bytes (`{{\"msg\":`): {tc_skel}");

            release_tool.notify_one();
            let (pr_reply, _) = read_reply_and_pre(&mut cr1, 4).await;
            assert!(pr_reply.get("error").is_none(), "{pr_reply}");

            drop((cw2, cr2));
            st2.await.unwrap();
            drop((cw1, cr1));
            st1.await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn baseline_pages_entries_within_single_message_cap() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            // Inject 6 large entries (~140 KiB each > 512 KiB single page cap) into the active session.
            {
                let lua = host.kernel.lua();
                let session = host.kernel.service("session").unwrap();
                let add_fn: mlua::Function = lua
                    .load(
                        r#"
                        local s = ...
                        local big = string.rep("x", 140 * 1024)
                        for i = 1, 6 do
                            s:append_compaction("sum-" .. i .. ":" .. big, nil, 100)
                        end
                        "#,
                    )
                    .into_function()
                    .unwrap();
                let _ = add_fn.call::<mlua::Value>(session);
            }

            let (mut cr, mut cw, st, sid) = open_test_conn(&host, true, 4 * 1024 * 1024).await;

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"session_id":sid}}))
                .await;
            let _ = read_reply_and_pre(&mut cr, 2).await;
            let snap = rpc_read(&mut cr).await;
            assert_eq!(snap["method"], "snapshot", "{snap}");
            let bid = snap["params"]["baseline_id"].as_str().unwrap().to_string();

            let mut pages = 0usize;
            let mut total_items = 0usize;
            let mut cursor: Option<String> = None;
            let mut id = 10u64;
            loop {
                let mut params = json!({ "baseline_id": bid, "part": "entries" });
                if let Some(c) = cursor.take() {
                    params["cursor"] = json!(c);
                }
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":id,"method":"baseline.read","params":params})).await;
                let (reply, _) = read_reply_and_pre(&mut cr, id).await;
                id += 1;
                let encoded_len = serde_json::to_vec(&reply).unwrap().len();
                assert!(encoded_len <= mare_kernel::BASELINE_PAGE_MAX_BYTES, "page exceeded cap: {encoded_len}");
                let arr = reply["result"]["data"].as_array().unwrap();
                total_items += arr.len();
                pages += 1;
                match reply["result"].get("next_cursor").and_then(J::as_str) {
                    Some(n) => cursor = Some(n.to_string()),
                    None => break,
                }
            }
            assert!(pages >= 2, "expected multiple pages, got {pages}");
            assert!(total_items >= 6, "expected >= 6 items, got {total_items}");

            drop((cw, cr));
            st.await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn baseline_read_protocol_method_limit_and_stale_error() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let proto_schema = mare_protocol::schema();
            let method = proto_schema["methods"]
                .get("baseline.read")
                .expect("baseline.read must be declared as a protocol method in mare-protocol");
            let params_props = &method["params"]["properties"];
            let limit_ty = &params_props["limit"]["type"];
            assert!(
                limit_ty == "integer"
                    || limit_ty.as_array().is_some_and(|a| a.iter().any(|v| v == "integer")),
                "{method}"
            );
            assert_eq!(params_props["limit"]["minimum"], 1, "{method}");
            assert!(
                method["result"].get("oneOf").is_none(),
                "baseline.read result must be plain {{data, next_cursor?}} without stale oneOf: {method}"
            );
            assert_eq!(method["result"]["properties"]["data"]["type"], "array", "{method}");
            assert!(method["result"]["properties"].get("status").is_none(), "{method}");

            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            assert!(
                host.kernel.service_method("baseline", "read").is_none(),
                "baseline contract must be deleted once baseline.read is a protocol method"
            );

            {
                let lua = host.kernel.lua();
                let session = host.kernel.service("session").unwrap();
                let add_fn: mlua::Function = lua
                    .load(
                        r#"
                        local s = ...
                        for i = 1, 5 do
                            s:persist_message({ message = { role = "user", content = { { type = "text", text = "msg-" .. i } } } })
                        end
                        "#,
                    )
                    .into_function()
                    .unwrap();
                add_fn.call::<()>(session).unwrap();
            }

            let (mut cr, mut cw, st, sid) = open_test_conn(&host, true, 1 << 20).await;
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"session_id":sid}}))
                .await;
            let _ = read_reply_and_pre(&mut cr, 2).await;
            let snap = rpc_read(&mut cr).await;
            assert_eq!(snap["method"], "snapshot", "{snap}");
            let bid = snap["params"]["baseline_id"].as_str().unwrap().to_string();

            // limit = 0 must be rejected as invalid_arguments.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"baseline.read","params":{"baseline_id":bid,"part":"entries","limit":0}}),
            )
            .await;
            let (bad_limit, _) = read_reply_and_pre(&mut cr, 3).await;
            assert_eq!(bad_limit["error"]["data"]["code"], "invalid_arguments", "{bad_limit}");

            // Fetch exact expected entry sequence from the session first.
            let expected_ids: Vec<String> = {
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"session.list_items","params":{"session_id":sid}}))
                    .await;
                let (rep, _) = read_reply_and_pre(&mut cr, 4).await;
                assert!(rep.get("error").is_none(), "{rep}");
                rep["result"]["data"]
                    .as_array()
                    .expect("items array")
                    .iter()
                    .filter_map(|item| item.get("entry_id").and_then(J::as_str).map(str::to_owned))
                    .collect()
            };
            assert!(expected_ids.len() >= 5, "expected at least 5 entries, got {}", expected_ids.len());

            // Page with limit = 2 across every entry, verifying exact page sizes, cursor sequence,
            // zero duplicates, and zero omissions.
            let mut page_sizes = Vec::new();
            let mut seen_cursors = std::collections::BTreeSet::new();
            let mut collected_ids = Vec::new();
            let mut cursor: Option<String> = None;
            let mut id = 10u64;
            loop {
                let mut params = json!({ "baseline_id": bid, "part": "entries", "limit": 2 });
                if let Some(c) = cursor.take() {
                    params["cursor"] = json!(c);
                }
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":id,"method":"baseline.read","params":params})).await;
                let (reply, _) = read_reply_and_pre(&mut cr, id).await;
                id += 1;
                assert!(reply.get("error").is_none(), "{reply}");
                let items = reply["result"]["data"].as_array().expect("data array");
                assert!(!items.is_empty() && items.len() <= 2, "limit=2 page must have 1..=2 items: {reply}");
                page_sizes.push(items.len());
                for item in items {
                    if let Some(eid) = item.get("entry_id").and_then(J::as_str) {
                        collected_ids.push(eid.to_owned());
                    }
                }
                match reply["result"].get("next_cursor").and_then(J::as_str) {
                    Some(c) => {
                        assert!(!c.is_empty(), "next_cursor must be non-empty: {reply}");
                        assert!(seen_cursors.insert(c.to_owned()), "cursor must not repeat: {c}");
                        assert_eq!(items.len(), 2, "non-final page must be full (limit=2): {reply}");
                        cursor = Some(c.to_owned());
                    }
                    None => break,
                }
            }
            let expected_pages = expected_ids.len().div_ceil(2);
            assert_eq!(
                page_sizes.len(),
                expected_pages,
                "expected {expected_pages} pages for {} entries (sizes={page_sizes:?})",
                expected_ids.len()
            );
            assert_eq!(seen_cursors.len(), expected_pages - 1, "expected exact cursor count");
            let unique_collected: std::collections::BTreeSet<&String> = collected_ids.iter().collect();
            assert_eq!(unique_collected.len(), collected_ids.len(), "no duplicate entries across pages: {collected_ids:?}");
            assert_eq!(collected_ids, expected_ids, "paged baseline entries must match session entries with zero omissions");

            // Unknown / stale handle must return error code `baseline.stale` instead of `{ status: "stale" }`.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":99,"method":"baseline.read","params":{"baseline_id":"nonexistent","part":"entries"}}),
            )
            .await;
            let (stale, _) = read_reply_and_pre(&mut cr, 99).await;
            assert_eq!(stale["error"]["data"]["code"], "baseline.stale", "{stale}");
            assert!(stale.get("result").is_none(), "{stale}");

            drop((cw, cr));
            st.await.unwrap();
        })
        .await;
}
