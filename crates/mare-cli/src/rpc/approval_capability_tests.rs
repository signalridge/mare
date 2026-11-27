//! `capabilities.approvals` follows its changes across re-`initialize` (rust-native-host step 4
//! review): turned on twice, the host is asked once per approval; turned off, it is asked no
//! more, and the approval it was being asked at that moment ends (a deny) instead of waiting for
//! an answer nobody gives. Driven over the real RPC server.

use super::tests::{HOST_LOCK, rpc_send};
use super::*;

type Reader = BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>;
type Writer = tokio::io::WriteHalf<tokio::io::DuplexStream>;

/// A human approval per scripted turn: `touch <file>` asks for full access in a read-only session.
fn turn(file: &str) -> J {
    json!({"tool_calls": [{"name": "bash", "arguments": {"command": format!("touch {file}"), "sandbox_permissions": "all", "justification": "test"}}]})
}

async fn start(dir: &std::path::Path, script: J) -> (Arc<Host>, tokio::task::JoinHandle<()>, Reader, Writer) {
    let host = Host::new(mare_host::HostOptions {
        cwd: dir.to_owned(),
        mode: mare_plugin::Mode::Print,
        config: json!({
            "model": "faux/echo",
            "plugins": {
                "permissions": { "preset": "read-only" },
                "provider-faux": { "script_json": script.to_string() }
            }
        }),
        load_init: false,
        project_trusted: false,
        args: json!({"no_session": true}),
        no_user_plugins: true,
    })
    .await
    .unwrap();
    let (client, server) = tokio::io::duplex(1 << 20);
    let (sr, sw) = tokio::io::split(server);
    let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
    let (cr, cw) = tokio::io::split(client);
    (host, server_task, BufReader::new(cr), cw)
}

async fn next(cr: &mut Reader, stage: &str) -> J {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(30), cr.read_line(&mut line))
        .await
        .unwrap_or_else(|_| panic!("no message while waiting for {stage}"))
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

async fn initialize(cr: &mut Reader, cw: &mut Writer, id: u64, approvals: bool) {
    rpc_send(
        cw,
        json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"approvals":approvals}}}),
    )
    .await;
    let answer = loop {
        let m = next(cr, "the initialize response").await;
        if m["id"] == id {
            break m;
        }
    };
    assert!(answer["error"].is_null() && !answer["result"].is_null(), "initialize was refused: {answer}");
}

async fn set_capabilities(cr: &mut Reader, cw: &mut Writer, id: u64, approvals: bool) {
    rpc_send(
        cw,
        json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":"connection.set_capabilities",
            "params":{"capabilities":{"callbacks":false,"ui":false,"approvals":approvals,"elicitation":false}}
        }),
    )
    .await;
    let answer = loop {
        let m = next(cr, "the connection.set_capabilities response").await;
        if m["id"] == id {
            break m;
        }
    };
    assert!(
        answer["error"].is_null() && !answer["result"].is_null(),
        "connection.set_capabilities was refused: {answer}"
    );
}

/// The host-side voters the approvals capability registers, per point.
fn approval_voters(host: &Host) -> [usize; 2] {
    ["tool/approval-host", "net/approval-host"].map(|point| host.kernel.voter_count(point))
}

fn is_turn_end(m: &J) -> bool {
    m["method"] == "event" && m["params"]["type"] == "turn/completed"
}

#[tokio::test]
async fn approvals_turned_on_twice_ask_the_host_once() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let (host, server_task, mut cr, mut cw) = start(dir.path(), json!([turn("ran"), {"text": "done"}])).await;
            let before = approval_voters(&host);
            let sid = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            initialize(&mut cr, &mut cw, 1, true).await;
            set_capabilities(&mut cr, &mut cw, 2, true).await;
            assert_eq!(
                approval_voters(&host),
                [before[0] + 1, before[1] + 1],
                "connection.set_capabilities approvals=true must keep one voter per point"
            );
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"agent.prompt","params":{"session_id":sid,"input":"go"}}),
            )
            .await;
            let mut asked = 0;
            loop {
                let m = next(&mut cr, "the approved turn's end").await;
                if m["method"] == "approval.request" {
                    asked += 1;
                    assert_eq!(asked, 1, "the same approval was forwarded to the host again: {m}");
                    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":m["id"],"result":{"decision":"allow"}})).await;
                }
                if is_turn_end(&m) {
                    break;
                }
            }
            assert_eq!(asked, 1, "the approval was never forwarded");
            assert!(dir.path().join("ran").exists(), "the approved command did not run");
            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn approvals_turned_off_end_the_pending_request_and_ask_no_more() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let script = json!([turn("first"), {"text": "done"}, turn("second"), {"text": "done"}]);
            let (host, server_task, mut cr, mut cw) = start(dir.path(), script).await;
            let before = approval_voters(&host);
            let sid = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            initialize(&mut cr, &mut cw, 1, true).await;
            assert_eq!(
                approval_voters(&host),
                [before[0] + 1, before[1] + 1],
                "approvals=true must register one voter per point"
            );
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid,"input":"go"}}),
            )
            .await;
            let ask = loop {
                let m = next(&mut cr, "approval.request").await;
                if m["method"] == "approval.request" {
                    break m;
                }
            };

            // withdrawn while the host is being asked: the request is cancelled (ahead of the
            // initialize answer) and the turn ends
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0",
                    "id":3,
                    "method":"connection.set_capabilities",
                    "params":{"capabilities":{"callbacks":false,"ui":false,"approvals":false,"elicitation":false}}
                }),
            )
            .await;
            let mut cancelled = false;
            loop {
                let m = next(&mut cr, "the end of the approval pending when approvals were withdrawn").await;
                assert_ne!(m["method"], "approval.request", "asked again after approvals were withdrawn: {m}");
                cancelled |= m["method"] == "$/cancel_request" && m["params"]["id"] == ask["id"];
                if is_turn_end(&m) {
                    break;
                }
            }
            assert!(cancelled, "the pending approval.request was not cancelled");
            // the voters themselves are gone, the connection still open (a left-over voter would
            // answer `unavailable` on its own and satisfy everything above; check the registration,
            // not only the outcome)
            assert_eq!(approval_voters(&host), before, "approvals=false left the host's approval voters registered");

            // and the next approval is decided without asking the host
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"agent.prompt","params":{"session_id":sid,"input":"again"}}),
            )
            .await;
            let mut decided = false;
            loop {
                let m = next(&mut cr, "the second turn's end").await;
                assert_ne!(m["method"], "approval.request", "asked after approvals were withdrawn: {m}");
                decided |= m["method"] == "event" && m["params"]["type"] == "approval/resolved";
                if is_turn_end(&m) {
                    break;
                }
            }
            assert!(decided, "the second turn reached no approval");
            assert!(!dir.path().join("first").exists() && !dir.path().join("second").exists());
            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn answerer_lease_replacement_revokes_previous_answerer_and_cancels_pending_without_silent_transfer() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let script = json!([turn("first"), {"text": "done"}, turn("second"), {"text": "done"}]);
            let (host, task1, mut cr1, mut cw1) = start(dir.path(), script).await;
            let before = approval_voters(&host);

            let sid = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            initialize(&mut cr1, &mut cw1, 1, true).await;
            assert_eq!(approval_voters(&host), [before[0] + 1, before[1] + 1]);

            // Turn 1 asks conn1 for approval and waits on conn1's response.
            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid,"input":"go"}}),
            )
            .await;
            let ask1 = loop {
                let m = next(&mut cr1, "conn1 approval.request").await;
                if m["method"] == "approval.request" {
                    break m;
                }
            };

            // Connect a second owner connection with approvals enabled while ask1 is still pending.
            let (client2, server2) = tokio::io::duplex(1 << 20);
            let (sr2, sw2) = tokio::io::split(server2);
            let task2 = tokio::task::spawn_local(serve(host.clone(), sr2, sw2, true, std::future::pending()));
            let (cr2, mut cw2) = tokio::io::split(client2);
            let mut cr2 = BufReader::new(cr2);
            initialize(&mut cr2, &mut cw2, 10, true).await;

            // Old answerer's pending request is cancelled (never silently transferred to conn2),
            // and host does not double-register approval voters.
            let mut cancelled_on_1 = false;
            loop {
                let m = next(&mut cr1, "conn1 cancel and turn1 end").await;
                cancelled_on_1 |= m["method"] == "$/cancel_request" && m["params"]["id"] == ask1["id"];
                if is_turn_end(&m) {
                    break;
                }
            }
            assert!(cancelled_on_1, "replacing the answerer must cancel conn1's pending approval.request");
            assert_eq!(
                approval_voters(&host),
                [before[0] + 1, before[1] + 1],
                "replacing answerer must keep exactly one set of host approval voters"
            );
            assert!(!dir.path().join("first").exists(), "revoked approval must deny first command");

            // Subsequent approvals go to conn2, not conn1.
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":11,"method":"agent.prompt","params":{"session_id":sid,"input":"second"}}),
            )
            .await;
            let mut asked_on_2 = 0;
            let mut wait_sent = false;
            loop {
                let m = next(&mut cr2, "conn2 approval.request and turn2 response").await;
                if m["id"] == 11 && !wait_sent {
                    assert!(m["error"].is_null(), "{m}");
                    let tid = m["result"]["turn_id"].as_str().unwrap();
                    rpc_send(
                        &mut cw2,
                        json!({"jsonrpc":"2.0","id":12,"method":"agent.wait","params":{"session_id":sid,"turn_id":tid}}),
                    )
                    .await;
                    wait_sent = true;
                    continue;
                }
                if m["method"] == "approval.request" {
                    asked_on_2 += 1;
                    rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":m["id"],"result":{"decision":"allow"}})).await;
                }
                if m["id"] == 12 {
                    break;
                }
            }
            assert_eq!(asked_on_2, 1, "new answerer conn2 must receive subsequent approval.request");
            assert!(dir.path().join("second").exists(), "approved second command must run");

            drop(cw1);
            drop(cw2);
            task1.abort();
            task2.abort();
            let _ = task1.await;
            let _ = task2.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn approval_request_aborts_without_sending_when_taken_over_during_flush_events() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let script = json!([turn("first"), {"text": "done"}, turn("second"), {"text": "done"}]);
            let (host, task1, mut cr1, mut cw1) = start(dir.path(), script).await;
            let sid = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            initialize(&mut cr1, &mut cw1, 1, true).await;

            struct BarrierReset;
            impl Drop for BarrierReset {
                fn drop(&mut self) {
                    super::forward_approvals::tests::clear_flush_gate();
                }
            }
            let _reset = BarrierReset;
            let (barrier_entered, barrier_release) = super::forward_approvals::tests::arm_flush_gate();

            // Trigger prompt on conn1; conn1 suspends inside flush_events().await after the pre-await check.
            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid,"input":"go"}}),
            )
            .await;
            tokio::time::timeout(Duration::from_secs(5), barrier_entered)
                .await
                .expect("conn1 must pause inside flush_events()")
                .unwrap();

            // Connect conn2 (Owner) and take over AnswererSlot while conn1 is inside flush_events().await.
            let (client2, server2) = tokio::io::duplex(1 << 20);
            let (sr2, sw2) = tokio::io::split(server2);
            let task2 = tokio::task::spawn_local(super::serve::serve_inner(
                RpcRuntime::from_host(&host).await,
                sr2,
                sw2,
                Principal::Owner { remote_admin: true },
                false,
                None,
                std::future::pending(),
            ));
            let (cr2, mut cw2) = tokio::io::split(client2);
            let mut cr2 = BufReader::new(cr2);
            initialize(&mut cr2, &mut cw2, 10, true).await;

            // Release conn1's flush_events() now that conn1 is no longer Answerer and caps.approvals is false.
            let _ = barrier_release.send(());

            // Drain conn1 through turn end: it must NEVER receive an approval.request after losing Answerer.
            let mut saw_approval_on_1 = false;
            loop {
                let m = next(&mut cr1, "conn1 turn1 end without approval.request").await;
                if m["method"] == "approval.request" {
                    saw_approval_on_1 = true;
                }
                if is_turn_end(&m) {
                    break;
                }
            }
            assert!(!saw_approval_on_1, "revoked conn1 must not receive approval.request after flush_events()");
            assert!(!dir.path().join("first").exists(), "revoked turn must fail closed without executing command");

            // New owner conn2 still handles subsequent approvals normally.
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":11,"method":"agent.prompt","params":{"session_id":sid,"input":"second"}}),
            )
            .await;
            let mut asked_on_2 = 0;
            let mut wait_sent = false;
            loop {
                let m = next(&mut cr2, "conn2 approval.request and turn2 response").await;
                if m["id"] == 11 && !wait_sent {
                    assert!(m["error"].is_null(), "{m}");
                    let tid = m["result"]["turn_id"].as_str().unwrap();
                    rpc_send(
                        &mut cw2,
                        json!({"jsonrpc":"2.0","id":12,"method":"agent.wait","params":{"session_id":sid,"turn_id":tid}}),
                    )
                    .await;
                    wait_sent = true;
                    continue;
                }
                if m["method"] == "approval.request" {
                    asked_on_2 += 1;
                    rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":m["id"],"result":{"decision":"allow"}})).await;
                }
                if m["id"] == 12 {
                    break;
                }
            }
            assert_eq!(asked_on_2, 1);
            assert!(dir.path().join("second").exists());

            drop(cw1);
            drop(cw2);
            task1.abort();
            task2.abort();
            let _ = task1.await;
            let _ = task2.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn answerer_disconnect_fails_pending_approval_closed_without_transferring_to_other_owner() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let script = json!([turn("first"), {"text": "done"}]);
            let (host, task_primary, mut cr_primary, mut cw_primary) = start(dir.path(), script).await;

            // Secondary Owner (owns_process_lifecycle = false) acquires AnswererSlot while primary stays
            // connected as an idle Owner without approvals.
            initialize(&mut cr_primary, &mut cw_primary, 1, false).await;

            let (client_ans, server_ans) = tokio::io::duplex(1 << 20);
            let (sr_ans, sw_ans) = tokio::io::split(server_ans);
            let task_ans = tokio::task::spawn_local(super::serve::serve_inner(
                RpcRuntime::from_host(&host).await,
                sr_ans,
                sw_ans,
                Principal::Owner { remote_admin: true },
                false,
                None,
                std::future::pending(),
            ));
            let (cr_ans, mut cw_ans) = tokio::io::split(client_ans);
            let mut cr_ans = BufReader::new(cr_ans);
            initialize(&mut cr_ans, &mut cw_ans, 10, true).await;
            assert!(host.answerer.holder().is_some());

            let sid = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            // Start a turn on primary; approval.request lands on secondary Answerer.
            rpc_send(
                &mut cw_primary,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid,"input":"go"}}),
            )
            .await;
            let ask = loop {
                let m = next(&mut cr_ans, "answerer approval.request").await;
                if m["method"] == "approval.request" {
                    break m;
                }
            };
            assert!(ask["id"].is_number() || ask["id"].is_string());

            // Disconnect the Answerer abruptly without answering; primary stays connected.
            drop(cw_ans);
            drop(cr_ans);
            tokio::time::timeout(Duration::from_secs(2), task_ans)
                .await
                .expect("secondary owner task should shut down promptly")
                .unwrap();

            // Primary Owner stays connected, does NOT silently inherit the pending approval, and
            // observes the turn finish fail-closed with AnswererSlot released.
            let mut primary_saw_approval = false;
            loop {
                let m = next(&mut cr_primary, "primary turn end after answerer disconnect").await;
                if m["method"] == "approval.request" {
                    primary_saw_approval = true;
                }
                if is_turn_end(&m) {
                    break;
                }
            }
            assert!(!primary_saw_approval, "pending approval must not silently transfer to another connected Owner");
            assert!(host.answerer.holder().is_none(), "disconnected answerer must release AnswererSlot");
            assert!(!dir.path().join("first").exists(), "disconnected answerer must deny pending command");

            drop(cw_primary);
            task_primary.abort();
            let _ = task_primary.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn peer_initialize_cannot_preempt_or_interfere_with_owner_answerer() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let script = json!([turn("first"), {"text": "done"}]);
            let (host, task_owner, mut cr_owner, mut cw_owner) = start(dir.path(), script).await;
            initialize(&mut cr_owner, &mut cw_owner, 1, true).await;
            let holder_before = host.answerer.holder().expect("owner holds AnswererSlot");

            // Connect a Peer and try to claim ui/approvals/elicitation capabilities.
            let (client_peer, server_peer) = tokio::io::duplex(1 << 20);
            let (sr_peer, sw_peer) = tokio::io::split(server_peer);
            let task_peer = tokio::task::spawn_local(serve_peer(host.clone(), sr_peer, sw_peer, None));
            let (cr_peer, mut cw_peer) = tokio::io::split(client_peer);
            let mut cr_peer = BufReader::new(cr_peer);
            rpc_send(
                &mut cw_peer,
                json!({
                    "jsonrpc": "2.0",
                    "id": 99,
                    "method": "initialize",
                    "params": {"protocol_version": "3.0", "capabilities": {"ui": true, "approvals": true, "elicitation": true}}
                }),
            )
            .await;
            let peer_init = loop {
                let m = next(&mut cr_peer, "peer initialize response").await;
                if m["id"] == 99 {
                    break m;
                }
            };
            let caps = &peer_init["result"]["capabilities"]["callbacks"];
            assert_eq!(caps["ui"], false);
            assert_eq!(caps["approvals"], false);
            assert_eq!(caps["elicitation"], false);
            assert_eq!(host.answerer.holder(), Some(holder_before), "Peer initialize must not touch AnswererSlot");

            let sid = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            // Owner still receives and approves the prompt's approval.request.
            rpc_send(
                &mut cw_owner,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid,"input":"go"}}),
            )
            .await;
            let mut wait_sent = false;
            loop {
                let m = next(&mut cr_owner, "owner approval and turn end").await;
                if m["id"] == 2 && !wait_sent {
                    assert!(m["error"].is_null(), "{m}");
                    let tid = m["result"]["turn_id"].as_str().unwrap();
                    rpc_send(
                        &mut cw_owner,
                        json!({"jsonrpc":"2.0","id":3,"method":"agent.wait","params":{"session_id":sid,"turn_id":tid}}),
                    )
                    .await;
                    wait_sent = true;
                    continue;
                }
                if m["method"] == "approval.request" {
                    rpc_send(&mut cw_owner, json!({"jsonrpc":"2.0","id":m["id"],"result":{"decision":"allow"}})).await;
                }
                if m["id"] == 3 {
                    break;
                }
            }
            assert!(dir.path().join("first").exists());

            drop(cw_peer);
            drop(cw_owner);
            task_peer.abort();
            task_owner.abort();
            let _ = task_peer.await;
            let _ = task_owner.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_set_capabilities_withdraws_then_reenables_safely() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let script = json!([turn("first"), {"text": "done"}, turn("second"), {"text": "done"}]);
            let (host, server_task, mut cr, mut cw) = start(dir.path(), script).await;
            let before = approval_voters(&host);
            let sid = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            initialize(&mut cr, &mut cw, 1, true).await;

            // Partial capabilities object is strictly rejected (-32602).
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":10,"method":"connection.set_capabilities","params":{"capabilities":{"approvals":false}}}),
            )
            .await;
            let partial_err = loop {
                let m = next(&mut cr, "partial set_capabilities error").await;
                if m["id"] == 10 {
                    break m;
                }
            };
            assert_eq!(partial_err["error"]["code"], -32602, "{partial_err}");

            // Trigger an in-flight approval, withdraw approvals via connection.set_capabilities.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid,"input":"go"}}),
            )
            .await;
            let ask = loop {
                let m = next(&mut cr, "approval.request").await;
                if m["method"] == "approval.request" {
                    break m;
                }
            };
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0",
                    "id":3,
                    "method":"connection.set_capabilities",
                    "params":{"capabilities":{"callbacks":false,"ui":false,"approvals":false,"elicitation":false}}
                }),
            )
            .await;
            let (mut got_reply, mut turn_ended) = (false, false);
            while !(got_reply && turn_ended) {
                let m = next(&mut cr, "turn completion and set_capabilities reply after approvals revocation").await;
                assert_ne!(m["method"], "approval.request", "unexpected second ask for {ask:?}: {m}");
                if m["id"] == 3 {
                    assert!(
                        m["error"].is_null() && !m["result"].is_null(),
                        "connection.set_capabilities was refused: {m}"
                    );
                    got_reply = true;
                }
                if is_turn_end(&m) {
                    turn_ended = true;
                }
            }
            assert!(!dir.path().join("first").exists(), "revoked approvals must deny first command");

            // A late allow reply to the withdrawn ask must never revive or run the command, even after re-enabling.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":ask["id"],"result":{"decision":"allow"}})).await;
            tokio::task::yield_now().await;

            // Re-enable approvals safely via connection.set_capabilities without duplicate voters.
            set_capabilities(&mut cr, &mut cw, 4, true).await;
            assert_eq!(approval_voters(&host), [before[0] + 1, before[1] + 1]);
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"agent.prompt","params":{"session_id":sid,"input":"go2"}}),
            )
            .await;
            loop {
                let m = next(&mut cr, "second approved turn").await;
                if m["method"] == "approval.request" {
                    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":m["id"],"result":{"decision":"allow"}})).await;
                }
                if is_turn_end(&m) {
                    break;
                }
            }
            assert!(!dir.path().join("first").exists(), "late allow after revocation must never execute first command");
            assert!(dir.path().join("second").exists(), "re-enabled approvals must execute approved command");

            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn peer_capabilities_cannot_steal_answerer() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let (host, task_owner, mut cr_owner, mut cw_owner) = start(dir.path(), json!([])).await;
            initialize(&mut cr_owner, &mut cw_owner, 1, true).await;
            let holder_before = host.answerer.holder().expect("owner holds AnswererSlot");

            let (client_peer, server_peer) = tokio::io::duplex(1 << 20);
            let (sr_peer, sw_peer) = tokio::io::split(server_peer);
            let task_peer = tokio::task::spawn_local(serve_peer(host.clone(), sr_peer, sw_peer, None));
            let (cr_peer, mut cw_peer) = tokio::io::split(client_peer);
            let mut cr_peer = BufReader::new(cr_peer);

            rpc_send(
                &mut cw_peer,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}}),
            )
            .await;
            loop {
                if next(&mut cr_peer, "peer init").await["id"] == 1 {
                    break;
                }
            }
            rpc_send(
                &mut cw_peer,
                json!({
                    "jsonrpc":"2.0",
                    "id":2,
                    "method":"connection.set_capabilities",
                    "params":{"capabilities":{"callbacks":true,"ui":true,"approvals":true,"elicitation":true}}
                }),
            )
            .await;
            let set_res = loop {
                let m = next(&mut cr_peer, "peer set_capabilities").await;
                if m["id"] == 2 {
                    break m;
                }
            };
            let caps = &set_res["result"]["capabilities"];
            assert_eq!(caps["ui"], false, "{set_res}");
            assert_eq!(caps["approvals"], false, "{set_res}");
            assert_eq!(caps["elicitation"], false, "{set_res}");
            assert_eq!(
                host.answerer.holder(),
                Some(holder_before),
                "peer set_capabilities must not steal AnswererSlot"
            );

            drop(cw_peer);
            drop(cw_owner);
            task_peer.abort();
            task_owner.abort();
            let _ = task_peer.await;
            let _ = task_owner.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn ui_attach_on_second_session_preserves_pending_approval_on_first_session() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir1 = tempfile::tempdir().unwrap();
            let dir2 = tempfile::tempdir().unwrap();
            let script = json!([turn("ran_after_attach"), { "text": "done" }]);
            let opts1 = mare_host::HostOptions {
                cwd: dir1.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": {
                        "permissions": { "preset": "read-only" },
                        "provider-faux": { "script_json": script.to_string() }
                    }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let opts2 = mare_host::HostOptions {
                cwd: dir2.path().to_owned(),
                config: json!({ "model": "faux/echo", "plugins": { "permissions": { "preset": "read-only" } } }),
                ..opts1.clone()
            };
            let manager = mare_host::HostManager::new();
            let rt1 = manager.create_session(opts1.clone()).await.unwrap();
            let rt2 = manager.create_session(opts2).await.unwrap();
            let s1 = rt1.session_id.clone();
            let s2 = rt2.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts1, rt1.host.clone(), Some(s1.clone()), false);

            let (client, server) = tokio::io::duplex(1 << 20);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocol_version": "3.0",
                        "capabilities": { "ui": true, "approvals": true }
                    }
                }),
            )
            .await;
            let init = loop {
                let m = next(&mut cr, "initialize reply").await;
                if m["id"] == 1 {
                    break m;
                }
            };
            assert!(init["error"].is_null(), "{init}");

            // Start a prompt on s1 that triggers a tool approval.request.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "agent.prompt",
                    "params": { "session_id": s1, "input": "go" }
                }),
            )
            .await;
            let mut turn_id = None;
            let ask = loop {
                let m = next(&mut cr, "tool approval.request on s1").await;
                if m["id"] == 2 {
                    turn_id = m["result"]["turn_id"].as_str().map(str::to_string);
                }
                if m["method"] == "approval.request" {
                    break m;
                }
            };
            assert_eq!(ask["params"]["kind"], "tool", "{ask}");
            assert_eq!(ask["params"]["session_id"], s1, "approval.request must carry session_id: {ask}");
            if let Some(tid) = turn_id.as_deref() {
                assert_eq!(ask["params"]["turn_id"], tid, "approval.request must carry active turn_id: {ask}");
            } else {
                assert!(ask["params"]["turn_id"].is_string(), "approval.request must carry turn_id: {ask}");
            }

            // While s1's approval.request is still pending, attach as responder on s2.
            // This must NOT invalidate s1's pending approval callback.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "ui.attach",
                    "params": { "session_id": s2, "responder": true }
                }),
            )
            .await;
            let attach_res = loop {
                let m = next(&mut cr, "ui.attach reply for s2").await;
                if m["id"] == 3 {
                    break m;
                }
            };
            assert!(attach_res["error"].is_null(), "{attach_res}");
            assert_eq!(attach_res["result"]["responder"], true, "{attach_res}");

            // Now approve s1's in-flight request; the decision must take effect and run the command.
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": ask["id"], "result": { "decision": "allow" } })).await;
            loop {
                let m = next(&mut cr, "s1 turn/completed after allow").await;
                if is_turn_end(&m) && m["params"]["session_id"] == s1 {
                    break;
                }
            }
            assert!(
                dir1.path().join("ran_after_attach").exists(),
                "approving s1 after ui.attach on s2 must execute the approved command"
            );

            // Also verify network approval.request carries session_id.
            let net_host = rt1.host.clone();
            let net_task = tokio::task::spawn_local(async move {
                let lua = net_host.lua();
                let req = lua.create_table().unwrap();
                req.set("host", "example.com").unwrap();
                req.set("port", 443).unwrap();
                req.set("reason", "network probe").unwrap();
                let mut args = mlua::MultiValue::new();
                args.push_back(mlua::Value::Table(req));
                net_host.kernel.policy("net/approval-host", args).await.unwrap()
            });
            let net_ask = loop {
                let m = next(&mut cr, "network approval.request on s1").await;
                if m["method"] == "approval.request" {
                    break m;
                }
            };
            assert_eq!(net_ask["params"]["kind"], "network", "{net_ask}");
            assert_eq!(
                net_ask["params"]["session_id"], s1,
                "network approval.request must carry session_id: {net_ask}"
            );
            assert_eq!(net_ask["params"]["host"], "example.com:443", "{net_ask}");
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": net_ask["id"], "result": { "decision": "allow" } }))
                .await;
            let _ = net_task.await.unwrap();

            drop((cw, cr));
            task.abort();
            let _ = task.await;
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn session_opened_without_capable_creator_binds_local_owner_answerer() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir1 = tempfile::tempdir().unwrap();
            let dir_lua = tempfile::tempdir().unwrap();
            let script = json!([turn("ran_fallback"), { "text": "done" }]);
            let base_opts = mare_host::HostOptions {
                cwd: dir1.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "webserver": { "remote_admin": false },
                    "plugins": {
                        "permissions": { "preset": "read-only" },
                        "provider-faux": { "script_json": script.to_string() }
                    }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt1 = manager.create_session(base_opts.clone()).await.unwrap();
            let s1 = rt1.session_id.clone();
            let rpc = RpcRuntime::new(manager, base_opts.clone(), rt1.host.clone(), Some(s1.clone()), false);

            // Connect local owner (can answer) and remote non-admin owner (cannot answer).
            let (local_client, local_server) = tokio::io::duplex(1 << 20);
            let (lsr, lsw) = tokio::io::split(local_server);
            let local_task =
                tokio::task::spawn_local(serve_manager(rpc.clone(), lsr, lsw, true, None, std::future::pending()));
            let (lcr, mut lcw) = tokio::io::split(local_client);
            let mut lcr = BufReader::new(lcr);
            initialize(&mut lcr, &mut lcw, 1, true).await;
            let local_holder = rt1.host.answerer.holder().expect("local owner holds s1 answerer");

            let (remote_client, remote_server) = tokio::io::duplex(1 << 20);
            let (rsr, rsw) = tokio::io::split(remote_server);
            let remote_task = tokio::task::spawn_local(super::serve::serve_inner(
                rpc.clone(),
                rsr,
                rsw,
                Principal::Owner { remote_admin: false },
                true,
                None,
                std::future::pending(),
            ));
            let (rcr, mut rcw) = tokio::io::split(remote_client);
            let mut rcr = BufReader::new(rcr);
            initialize(&mut rcr, &mut rcw, 1, true).await;

            // (a) Non-admin remote owner creates a session inside the current project: creator
            // fails Op::Answerer, so the local owner declaring approvals must be bound as fallback answerer.
            rpc_send(
                &mut rcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.create",
                    "params": { "cwd": dir1.path().to_string_lossy() }
                }),
            )
            .await;
            let created = loop {
                let m = next(&mut rcr, "remote session.create reply").await;
                if m["id"] == 2 {
                    break m;
                }
            };
            assert!(created["error"].is_null(), "{created}");
            let s_remote = created["result"]["session_id"].as_str().unwrap().to_string();
            let rt_remote = rpc.manager.session(&s_remote).unwrap();
            assert_eq!(
                rt_remote.host.answerer.holder(),
                Some(local_holder),
                "session created by non-admin remote owner must bind local owner as fallback answerer"
            );

            // (b) Session opened with no RPC creator (Lua public API path): local owner must also be bound.
            let lua_opts = mare_host::HostOptions { cwd: dir_lua.path().to_owned(), ..base_opts };
            let rt_lua = rpc.manager.create_session(lua_opts).await.unwrap();
            let s_lua = rt_lua.session_id.clone();
            rpc.on_session_opened(&rt_lua, None, false).await;
            assert_eq!(
                rt_lua.host.answerer.holder(),
                Some(local_holder),
                "Lua-opened session (creator=None) must bind local owner as fallback answerer"
            );

            // Trigger an approval on the Lua-opened session and verify local owner receives and approves it.
            rpc_send(
                &mut lcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "agent.prompt",
                    "params": { "session_id": s_lua, "input": "go" }
                }),
            )
            .await;
            loop {
                let m = next(&mut lcr, "approval and turn completion on Lua-opened session").await;
                if m["method"] == "approval.request" {
                    assert_eq!(m["params"]["session_id"], s_lua, "{m}");
                    rpc_send(&mut lcw, json!({ "jsonrpc": "2.0", "id": m["id"], "result": { "decision": "allow" } }))
                        .await;
                }
                if is_turn_end(&m) && m["params"]["session_id"] == s_lua {
                    break;
                }
            }
            assert!(
                dir_lua.path().join("ran_fallback").exists(),
                "local owner fallback answerer must approve tool calls on Lua-opened session"
            );

            drop((rcw, rcr));
            drop((lcw, lcr));
            remote_task.abort();
            local_task.abort();
            let _ = remote_task.await;
            let _ = local_task.await;
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}
