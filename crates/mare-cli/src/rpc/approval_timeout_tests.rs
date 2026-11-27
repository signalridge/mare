//! A forwarded human approval is bound by
//! `approval_timeout_ms` (none by default), never by the ordinary `callback_timeout_ms`. Driven over
//! the real RPC server with a paused clock: once approval.request arrives, the clock jumps far
//! past the 30 ms callback timeout; the test then wakes the approval's own wait and waits until
//! it was polled at the new time (`wait_probe`) — a wait bound by the callback timeout is over
//! at that poll (tokio's `timeout` sees its deadline passed), the right one is still pending.
//! Then no `$/cancel_request` may have been sent, `agent.get` must report the turn busy and not
//! cancelled, and answering the original request must still run the command.

use super::super::wait_probe::WAITS;
use super::tests::{HOST_LOCK, rpc_read, rpc_send};
use super::*;

#[tokio::test(start_paused = true)]
async fn a_forwarded_approval_outlives_the_callback_timeout_and_can_still_be_answered() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let script = json!([
                {"tool_calls": [{"name": "bash", "arguments": {"command": "touch approved-ran", "sandbox_permissions": "all", "justification": "test"}}]},
                {"text": "done"}
            ]);
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
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
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let sid = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0", "capabilities":{"approvals":true},"callback_timeout_ms":30}})).await;
            while rpc_read(&mut cr).await["id"] != 1 {}
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid,"input":"go"}})).await;
            let ask = loop {
                let m = rpc_read(&mut cr).await;
                if m["method"] == "approval.request" {
                    break m;
                }
            };

            // the approval's wait was registered when the request was sent
            let approval = WAITS.with_borrow(|w| {
                let i: Vec<usize> = (0..w.len()).filter(|&i| w[i].method == methods::APPROVAL_REQUEST).collect();
                assert_eq!(i.len(), 1, "one approval wait");
                i[0]
            });
            // far past callback_timeout_ms; then drive that wait at the new time ourselves
            tokio::time::advance(Duration::from_secs(60)).await;
            let jump = tokio::time::Instant::now();
            WAITS.with_borrow(|w| w[approval].waker.clone()).expect("the approval wait was never polled").wake();
            let mut spins = 0;
            while !WAITS.with_borrow(|w| w[approval].polls.iter().any(|(at, _)| *at >= jump)) {
                spins += 1;
                assert!(spins < 100_000, "the woken approval wait was never polled");
                tokio::task::yield_now().await;
            }
            let finished = WAITS.with_borrow(|w| w[approval].polls.iter().any(|(_, ready)| *ready));
            assert!(!finished, "the approval wait ended at callback_timeout_ms (polled after the clock jump)");
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":99,"method":"agent.get","params":{"session_id":sid}})).await;
            let state = loop {
                let m = rpc_read(&mut cr).await;
                assert_ne!(m["method"], "$/cancel_request", "the approval was cancelled at callback_timeout_ms: {m}");
                assert!(!(m["method"] == "event" && m["params"]["type"] == "turn/completed"), "the turn ended: {m}");
                if m["id"] == 99 {
                    break m;
                }
            };
            assert!(state.get("error").is_none(), "{state}");
            assert_eq!(state["result"]["busy"], true, "the turn must still be waiting for the approval: {state}");
            assert_eq!(state["result"]["turn_cancelled"], false, "{state}");

            // the original request is still live: answering it runs the command
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":ask["id"],"result":{"decision":"allow"}})).await;
            loop {
                let m = rpc_read(&mut cr).await;
                if m["method"] == "event" && m["params"]["type"] == "turn/completed" {
                    break;
                }
            }
            assert!(dir.path().join("approved-ran").exists(), "the late-but-valid approval did not run the command");
            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}
