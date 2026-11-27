//! Pre-admission RPC prompt race tests (using `Host` + `serve` over duplex with an `agent.prompt`
//! gate instead of `frontend/prompt`).

use std::sync::atomic::{AtomicUsize, Ordering};

use super::tests::{HOST_LOCK, rpc_read, rpc_send};
use super::*;

type Reader = BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>;
type Writer = tokio::io::WriteHalf<tokio::io::DuplexStream>;

struct RaceRig {
    host: Arc<Host>,
    server: tokio::task::JoinHandle<()>,
    cr: Reader,
    cw: Writer,
    _dir: tempfile::TempDir,
}

async fn start_race_rig(script: J, nth_gate: usize) -> (RaceRig, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new(mare_host::HostOptions {
        cwd: dir.path().to_owned(),
        mode: mare_plugin::Mode::Print,
        config: json!({
            "model": "faux/echo",
            "plugins": {
                "permissions": { "enabled": false },
                "provider-faux": { "script_json": script.to_string() },
            },
        }),
        load_init: false,
        project_trusted: false,
        args: json!({}),
        no_user_plugins: true,
    })
    .await
    .unwrap();

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    {
        let agent_tbl = host.service_table("agent").unwrap();
        let orig: mlua::Function = agent_tbl.get("prompt").unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let entered_c = entered.clone();
        let release_c = release.clone();
        let wrapped = host
            .lua()
            .create_async_function(move |_, args: mlua::MultiValue| {
                let orig = orig.clone();
                let seen = seen.clone();
                let entered = entered_c.clone();
                let release = release_c.clone();
                async move {
                    if seen.fetch_add(1, Ordering::SeqCst) + 1 == nth_gate {
                        entered.notify_one();
                        release.notified().await;
                    }
                    orig.call_async::<mlua::MultiValue>(args).await
                }
            })
            .unwrap();
        agent_tbl.set("prompt", wrapped).unwrap();
    }

    let (client, server) = tokio::io::duplex(1 << 20);
    let (sr, sw) = tokio::io::split(server);
    let server = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
    let (cr, cw) = tokio::io::split(client);
    let rig = RaceRig { host, server, cr: BufReader::new(cr), cw, _dir: dir };
    (rig, entered, release)
}

async fn finish_rig(rig: RaceRig) {
    rig.server.abort();
    let _ = rig.server.await;
    rig.host.shutdown().await;
}

#[derive(Default)]
struct Events {
    starts: usize,
    ends: usize,
    streams: usize,
    types: Vec<String>,
}

impl Events {
    fn note(&mut self, m: &J) {
        if m.get("method").and_then(J::as_str) == Some("event") {
            let kind = m["params"]["type"].as_str().unwrap_or("?").to_string();
            match kind.as_str() {
                "turn/started" => self.starts += 1,
                "turn/completed" => self.ends += 1,
                "item/delta" => self.streams += 1,
                _ => {}
            }
            self.types.push(kind);
        }
    }

    async fn wait_reply(&mut self, cr: &mut Reader, id: &str) -> J {
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(5), cr.read_line(&mut line))
                .await
                .unwrap_or_else(|e| panic!("timeout waiting for `{id}`: {e}"))
                .unwrap();
            assert!(n > 0, "EOF waiting for `{id}`, events={:?}", self.types);
            let m: J = serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad json for `{id}` ({line:?}): {e}"));
            assert_ne!(
                m.get("method").and_then(J::as_str),
                Some("stream.failed"),
                "stream.failed waiting for `{id}` (events={:?}): {m}",
                self.types
            );
            if m.get("method").is_none() && m["id"] == id {
                return m;
            }
            self.note(&m);
        }
    }
}

async fn call_req(cr: &mut Reader, cw: &mut Writer, seen: &mut Events, id: &str, method: &str, params: J) -> J {
    rpc_send(cw, json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await;
    seen.wait_reply(cr, id).await
}

#[tokio::test]
async fn rpc_prompt_session_hold_blocks_concurrent_session_switch() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = HOST_LOCK.lock().await;
            let (mut rig, entered, release) = start_race_rig(json!([{ "text": "ran once" }]), 1).await;
            let mut seen = Events::default();
            let init =
                call_req(&mut rig.cr, &mut rig.cw, &mut seen, "init", "initialize", json!({"protocol_version": "3.0"}))
                    .await;
            assert!(init["error"].is_null(), "{init}");
            call_req(
                &mut rig.cr,
                &mut rig.cw,
                &mut seen,
                "sub",
                "events.subscribe",
                json!({ "include": ["turn/started", "turn/completed"] }),
            )
            .await;
            let old = call_req(&mut rig.cr, &mut rig.cw, &mut seen, "snew1", "session.create", json!({})).await;
            let old_id = old["result"]["session_id"].as_str().expect("session.create returns session_id").to_string();

            let params = json!({ "session_id": old_id, "input": "run me", "request_id": "req-session-hold" });
            rpc_send(&mut rig.cw, json!({"jsonrpc":"2.0","id":"req-a","method":"agent.prompt","params":params})).await;
            entered.notified().await;

            // Host::bind_session_call prevents concurrent session switch/load while agent.prompt is paused in flight.
            let switch_busy = call_req(&mut rig.cr, &mut rig.cw, &mut seen, "snew2", "session.create", json!({})).await;
            assert_eq!(switch_busy["error"]["data"]["code"], "busy", "{switch_busy}");
            release.notify_one();

            let resp_a = seen.wait_reply(&mut rig.cr, "req-a").await;
            assert_eq!(resp_a["result"]["status"], "started", "{resp_a}");
            while seen.ends < 1 {
                let m = rpc_read(&mut rig.cr).await;
                seen.note(&m);
            }
            let again = call_req(&mut rig.cr, &mut rig.cw, &mut seen, "again", "agent.prompt", params).await;
            assert_eq!(again["result"]["duplicate"], true, "{again}");
            assert_eq!(seen.starts, 1, "the prompt ran once");
            let switched = call_req(&mut rig.cr, &mut rig.cw, &mut seen, "snew3", "session.create", json!({})).await;
            assert_ne!(switched["result"]["session_id"].as_str(), Some(old_id.as_str()), "{switched}");
            finish_rig(rig).await;
        })
        .await;
}

#[tokio::test]
async fn rpc_cross_request_agent_start_does_not_forge_started_receipt() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = HOST_LOCK.lock().await;
            let (mut rig, entered, release) = start_race_rig(
                json!([
                    { "text": "B slow ".repeat(300), "delay_ms": 20 },
                    { "text": "unexpected A reply" }
                ]),
                1,
            )
            .await;
            let voter = rig
                .host
                .lua()
                .load(
                    r#"return function(ev)
                        if ev and ev.inbox and ev.inbox.follow_up then
                            for _, item in ipairs(ev.inbox.follow_up) do
                                if item.text == "req-A" then
                                    return { decision = "deny", reason = "denied after B started" }
                                end
                            end
                        end
                        return { decision = "allow" }
                    end"#,
                )
                .eval()
                .unwrap();
            let ctx = rig.host.kernel.host_ctx().unwrap();
            let owner = rig.host.kernel.host_owner().unwrap();
            rig.host.kernel.host_voter(&ctx, owner, "session/commit-ready", voter, mare_kernel::HostVoter::default()).unwrap();

            let mut seen = Events::default();
            call_req(&mut rig.cr, &mut rig.cw, &mut seen, "init", "initialize", json!({"protocol_version": "3.0"})).await;
            call_req(
                &mut rig.cr,
                &mut rig.cw,
                &mut seen,
                "sub",
                "events.subscribe",
                json!({ "include": ["item/delta", "turn/completed"] }),
            )
            .await;
            let s1 = call_req(&mut rig.cr, &mut rig.cw, &mut seen, "s1", "session.create", json!({})).await;
            let sid = s1["result"]["session_id"].as_str().unwrap().to_string();

            rpc_send(
                &mut rig.cw,
                json!({"jsonrpc":"2.0","id":"req-a","method":"agent.prompt","params":{ "session_id": sid, "input": "req-A", "when_busy": "queue", "request_id": "req-A-cross" }}),
            )
            .await;
            entered.notified().await;

            let resp_b = call_req(
                &mut rig.cr,
                &mut rig.cw,
                &mut seen,
                "req-b",
                "agent.prompt",
                json!({ "session_id": sid, "input": "req-B", "request_id": "req-B-cross" }),
            )
            .await;
            assert_eq!(resp_b["result"]["status"], "started", "req-B must succeed: {resp_b}");
            while seen.streams < 1 {
                let m = rpc_read(&mut rig.cr).await;
                seen.note(&m);
            }
            release.notify_one();

            let resp_a = seen.wait_reply(&mut rig.cr, "req-a").await;
            assert!(resp_a.get("error").is_some(), "req-A must be rejected: {resp_a}");
            let retry_a = call_req(
                &mut rig.cr,
                &mut rig.cw,
                &mut seen,
                "retry-a",
                "agent.prompt",
                json!({ "session_id": sid, "input": "req-A", "when_busy": "queue", "request_id": "req-A-cross" }),
            )
            .await;
            assert!(
                retry_a.get("error").is_some(),
                "retry of denied req-A must fail, not return forged started receipt: {retry_a}"
            );
            let _ = call_req(&mut rig.cr, &mut rig.cw, &mut seen, "abort", "agent.cancel", json!({ "session_id": sid })).await;
            finish_rig(rig).await;
        })
        .await;
}

#[tokio::test]
async fn attached_load_with_agent_wait_in_flight_behaves_as_before() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = HOST_LOCK.lock().await;
            let (mut rig, entered, release) = start_race_rig(
                json!([{ "text": "persist me" }, { "text": "slow turn ".repeat(40), "delay_ms": 150 }]),
                2,
            )
            .await;
            let mut seen = Events::default();
            let init =
                call_req(&mut rig.cr, &mut rig.cw, &mut seen, "init", "initialize", json!({"protocol_version": "3.0"}))
                    .await;
            assert!(init["error"].is_null(), "{init}");
            let s1 = call_req(&mut rig.cr, &mut rig.cw, &mut seen, "s1", "session.create", json!({})).await;
            let sid = s1["result"]["session_id"].as_str().expect("session_id").to_string();

            // Complete turn 1 so `sid` is persisted on disk and resolvable by `session.load`.
            let p1 = call_req(
                &mut rig.cr,
                &mut rig.cw,
                &mut seen,
                "prompt-1",
                "agent.prompt",
                json!({ "session_id": sid, "input": "persist" }),
            )
            .await;
            assert_eq!(p1["result"]["status"], "started", "{p1}");
            let w1 = call_req(
                &mut rig.cr,
                &mut rig.cw,
                &mut seen,
                "wait-1",
                "agent.wait",
                json!({ "session_id": sid, "turn_id": p1["result"]["turn_id"] }),
            )
            .await;
            assert!(w1["error"].is_null(), "{w1}");

            // Start turn 2 (gated at prompt entry, then slow streaming) so agent.wait holds a bound session call.
            rpc_send(
                &mut rig.cw,
                json!({"jsonrpc":"2.0","id":"prompt-2","method":"agent.prompt","params":{ "session_id": sid, "input": "slow" }}),
            )
            .await;
            entered.notified().await;
            release.notify_one();
            let p2 = seen.wait_reply(&mut rig.cr, "prompt-2").await;
            assert_eq!(p2["result"]["status"], "started", "{p2}");
            let turn_id = p2["result"]["turn_id"].as_str().expect("turn_id").to_string();

            rpc_send(
                &mut rig.cw,
                json!({"jsonrpc":"2.0","id":"wait-2","method":"agent.wait","params":{ "session_id": sid, "turn_id": turn_id }}),
            )
            .await;
            rpc_send(
                &mut rig.cw,
                json!({"jsonrpc":"2.0","id":"load-1","method":"session.load","params":{ "session_id": sid }}),
            )
            .await;

            let mut wait_rep = None;
            let mut load_rep = None;
            while wait_rep.is_none() || load_rep.is_none() {
                let m = tokio::time::timeout(std::time::Duration::from_secs(5), rpc_read(&mut rig.cr))
                    .await
                    .expect("attached session.load with agent.wait in flight must not deadlock");
                if m.get("method").is_none() {
                    if m["id"] == "wait-2" {
                        wait_rep = Some(m);
                    } else if m["id"] == "load-1" {
                        load_rep = Some(m);
                    }
                }
            }
            let wait_rep = wait_rep.unwrap();
            let load_rep = load_rep.unwrap();
            assert!(wait_rep["error"].is_null(), "agent.wait must finish cleanly: {wait_rep}");
            assert_eq!(
                load_rep["error"]["data"]["code"], "busy",
                "session.load during in-flight agent.wait must be refused as busy: {load_rep}"
            );
            // Once agent.wait completes, session.load succeeds.
            let load_after = call_req(
                &mut rig.cr,
                &mut rig.cw,
                &mut seen,
                "load-2",
                "session.load",
                json!({ "session_id": sid }),
            )
            .await;
            assert_eq!(load_after["result"]["session_id"], sid, "{load_after}");
            finish_rig(rig).await;
        })
        .await;
}

#[tokio::test]
async fn switch_is_refused_while_a_bound_call_is_in_flight() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = HOST_LOCK.lock().await;
            let (mut rig, entered, release) = start_race_rig(json!([{ "text": "bound ok" }]), 1).await;
            // Remove any Lua `hold` helper from the session table to prove binding lives in Host::bind_session_call().
            {
                let session_tbl = rig.host.service_table("session").unwrap();
                session_tbl.set("hold", mlua::Value::Nil).unwrap();
            }
            let mut seen = Events::default();
            let init =
                call_req(&mut rig.cr, &mut rig.cw, &mut seen, "init", "initialize", json!({"protocol_version": "3.0"}))
                    .await;
            assert!(init["error"].is_null(), "{init}");
            let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();

            rpc_send(
                &mut rig.cw,
                json!({"jsonrpc":"2.0","id":"bound-1","method":"agent.prompt","params":{ "session_id": sid, "input": "hold me" }}),
            )
            .await;
            entered.notified().await;

            // Even with `session.hold` stripped, `kernel.begin_session_switch` is refused with `busy`
            // because `rpc/services.rs` holds `Host::bind_session_call()`.
            let kernel_tbl = rig.host.service_table("kernel").unwrap();
            rig.host.lua().globals().set("TEST_KERNEL", kernel_tbl).unwrap();
            let switch_err: String = rig
                .host
                .lua()
                .load(
                    r#"
                    local ok, err = pcall(function()
                        return TEST_KERNEL:begin_session_switch({ session_id = "blocked-target" })
                    end)
                    return tostring(ok) .. "|" .. tostring(type(err) == "table" and err.code or err)
                    "#,
                )
                .eval_async()
                .await
                .unwrap();
            assert!(
                switch_err.starts_with("false|busy"),
                "begin_session_switch must be refused as busy while bound call is in flight (even without session.hold): {switch_err}"
            );

            release.notify_one();
            let resp = seen.wait_reply(&mut rig.cr, "bound-1").await;
            assert_eq!(resp["result"]["status"], "started", "{resp}");
            let _ = call_req(&mut rig.cr, &mut rig.cw, &mut seen, "cancel", "agent.cancel", json!({ "session_id": sid })).await;
            finish_rig(rig).await;
        })
        .await;
}

#[tokio::test]
async fn bound_call_during_a_switch_is_stale() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = HOST_LOCK.lock().await;
            let (mut rig, _entered, _release) = start_race_rig(json!([]), 99).await;
            // Remove any Lua `hold` helper from the session table to prove stale_session comes from Host identity state.
            {
                let session_tbl = rig.host.service_table("session").unwrap();
                session_tbl.set("hold", mlua::Value::Nil).unwrap();
            }
            let mut seen = Events::default();
            let init =
                call_req(&mut rig.cr, &mut rig.cw, &mut seen, "init", "initialize", json!({"protocol_version": "3.0"}))
                    .await;
            assert!(init["error"].is_null(), "{init}");
            let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();

            // Open a host session switch transaction via `kernel.begin_session_switch` inside a live
            // coroutine so the origin-thread liveness guard sees an active switch until released.
            let kernel_tbl = rig.host.service_table("kernel").unwrap();
            rig.host.lua().globals().set("TEST_KERNEL", kernel_tbl).unwrap();
            rig.host
                .lua()
                .load(
                    r#"
                    local ready_tx, ready_rx = mare.task.channel()
                    local rel_tx, rel_rx = mare.task.channel()
                    TEST_SWITCH_REL = rel_tx
                    mare.task.spawn(function()
                        local sw <close> = TEST_KERNEL:begin_session_switch({ session_id = "in-progress-target" })
                        ready_tx:send(true)
                        rel_rx:recv()
                        sw:abort()
                    end)
                    assert(mare.task.timeout(5000, function() return ready_rx:recv() end))
                    "#,
                )
                .exec_async()
                .await
                .unwrap();

            // A session-scoped RPC call arriving during the switch must fail with `stale_session`.
            let resp = call_req(
                &mut rig.cr,
                &mut rig.cw,
                &mut seen,
                "during-switch",
                "agent.list_messages",
                json!({ "session_id": sid }),
            )
            .await;
            assert_eq!(
                resp["error"]["data"]["code"], "stale_session",
                "bound call arriving during an active session switch must return stale_session (even without session.hold): {resp}"
            );

            // Aborting the switch allows subsequent session-scoped calls to succeed again.
            rig.host
                .lua()
                .load("TEST_SWITCH_REL:send(true); TEST_SWITCH_REL = nil; mare.task.sleep(10)")
                .exec_async()
                .await
                .unwrap();
            let after = call_req(
                &mut rig.cr,
                &mut rig.cw,
                &mut seen,
                "after-abort",
                "agent.list_messages",
                json!({ "session_id": sid }),
            )
            .await;
            assert!(after["error"].is_null(), "bound call after switch abort must succeed: {after}");
            finish_rig(rig).await;
        })
        .await;
}
