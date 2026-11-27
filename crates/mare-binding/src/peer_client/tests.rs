use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn pair()
-> (PeerClient, BufReader<tokio::io::ReadHalf<tokio::net::UnixStream>>, tokio::io::WriteHalf<tokio::net::UnixStream>) {
    let path = tempfile::tempdir().unwrap();
    let socket = path.path().join("s");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (conn, accepted) =
        tokio::join!(crate::net_unix::connect(socket.to_str().unwrap(), Duration::from_secs(1)), listener.accept());
    let (read, write) = tokio::io::split(accepted.unwrap().0);
    let mut client = PeerClient::from_connection(conn.unwrap(), socket.to_string_lossy().into(), Default::default());
    client.info = json!({"allowed":["events.subscribe"]});
    (client, BufReader::new(read), write)
}
async fn read(read: &mut BufReader<tokio::io::ReadHalf<tokio::net::UnixStream>>) -> Value {
    let mut line = String::new();
    read.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}
async fn write(write: &mut tokio::io::WriteHalf<tokio::net::UnixStream>, value: Value) {
    write.write_all(format!("{value}\n").as_bytes()).await.unwrap();
}

#[tokio::test]
async fn mux_handles_reversed_responses_and_early_receipt_without_consuming_it() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (client, mut r, mut w) = pair().await;
            let calls = async {
                tokio::join!(
                    client.status(WaitOptions::default()),
                    client.send("hello".into(), false, WaitOptions::default())
                )
            };
            let server = async {
                let a = read(&mut r).await;
                let b = read(&mut r).await;
                let (status, send) = if a["method"] == "status.get" { (a, b) } else { (b, a) };
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","method":"peer.message_status","params":{"message_id":"m1","status":"delivered"}}),
                )
                .await;
                write(&mut w, json!({"jsonrpc":"2.0","id":send["id"],"result":{"message_id":"m1","status":"held"}})).await;
                write(&mut w, json!({"jsonrpc":"2.0","id":status["id"],"result":{"state":"working"}})).await;
            };
            let ((status, sent), ()) = tokio::join!(calls, server);
            assert_eq!(status.unwrap()["state"], "working");
            assert_eq!(sent.unwrap()["status"], "held");
            for _ in 0..2 {
                assert_eq!(client.wait_message("m1", WaitOptions::default()).await.unwrap()["status"], "delivered");
            }
            client.close().await;
        })
        .await;
}

#[tokio::test]
async fn cancelled_wait_preserves_receipt_and_late_send_is_unknown_without_retry() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (client, mut r, mut w) = pair().await;
            let send = client.send("hello".into(), false, WaitOptions::default());
            let server = async {
                let call = read(&mut r).await;
                write(&mut w, json!({"jsonrpc":"2.0","id":call["id"],"result":{"message_id":"m","status":"held"}}))
                    .await;
            };
            let (result, ()) = tokio::join!(send, server);
            result.unwrap();
            let cancel = CancellationToken::new();
            cancel.cancel();
            let opts = WaitOptions { timeout: None, cancel };
            assert_eq!(client.wait_message("m", opts).await.unwrap_err().code, "cancelled");
            let opts = WaitOptions { timeout: Some(Duration::from_millis(5)), ..Default::default() };
            assert_eq!(client.wait_message("m", opts.clone()).await.unwrap_err().code, "timeout");
            let cancel = CancellationToken::new();
            let waiting = client.wait_message("m", WaitOptions { timeout: None, cancel: cancel.clone() });
            let stop = async {
                tokio::task::yield_now().await;
                cancel.cancel();
            };
            let (stopped, ()) = tokio::join!(waiting, stop);
            assert_eq!(stopped.unwrap_err().code, "cancelled");

            write(
                &mut w,
                json!({"jsonrpc":"2.0","method":"peer.message_status","params":{"message_id":"m","status":"rejected"}}),
            )
            .await;
            assert_eq!(client.wait_message("m", WaitOptions::default()).await.unwrap()["status"], "rejected");
            let send = client.send("second".into(), false, opts);
            let server = async {
                let call = read(&mut r).await;
                assert_eq!(call["params"]["text"], "second");
            };
            let (result, ()) = tokio::join!(send, server);
            assert_eq!(result.unwrap_err().code, "outcome_unknown");
            assert!(tokio::time::timeout(Duration::from_millis(5), read(&mut r)).await.is_err(), "must not retry");
            client.close().await;
        })
        .await;
}

#[tokio::test]
async fn status_changed_notification_satisfies_wait_idle_and_close_wakes_waiters() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (client, mut r, mut w) = pair().await;
            let idle = client.wait_idle(false, WaitOptions::default());
            let server = async {
                let st0 = read(&mut r).await;
                assert_eq!(st0["method"], "status.get");
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":st0["id"],"result":{"state":"working","seq":1,"session":{"session_id":"s-live"}}}),
                )
                .await;
                let sub = read(&mut r).await;
                assert_eq!(sub["method"], "events.subscribe");
                assert_eq!(sub["params"]["include"], json!(["status/changed"]));
                assert_eq!(sub["params"]["session_id"], json!("s-live"));
                write(&mut w, json!({"jsonrpc":"2.0","id":sub["id"],"result":{"mode":"live","session_epoch":"e1","seq":1}})).await;
                let status = read(&mut r).await;
                assert_eq!(status["method"], "status.get");
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":status["id"],"result":{"state":"working","seq":1,"session":{"session_id":"s-live"}}}),
                )
                .await;
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","method":"event","params":{"session_id":"s-live","type":"status/changed","data":{"state":"exited","seq":2},"seq":2}}),
                )
                .await;
            };
            let (idle, ()) = tokio::join!(idle, server);
            let idle = idle.unwrap();
            assert_eq!(idle["state"], "exited");
            assert_eq!(idle["seq"], 2);
            let pending = client.status(WaitOptions { timeout: None, ..Default::default() });
            let shutdown = async {
                let _ = read(&mut r).await;
                tokio::join!(client.close(), client.close());
                assert!(*client.inner.finished.borrow());
            };
            let (result, ()) = tokio::join!(pending, shutdown);
            assert_eq!(result.unwrap_err().code, "closed");
            client.close().await;
        })
        .await;
}

#[tokio::test]
async fn reader_panic_close_notifies_changed_and_wakes_idle_waiter() {
    use std::future::Future;
    use std::task::Poll;

    tokio::task::LocalSet::new()
        .run_until(async {
            let (client, mut r, mut w) = pair().await;
            let idle = client.wait_idle(false, WaitOptions { timeout: None, ..Default::default() });
            tokio::pin!(idle);
            let server = async {
                let st0 = read(&mut r).await;
                assert_eq!(st0["method"], "status.get");
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":st0["id"],"result":{"state":"working","seq":1,"session":{"session_id":"s-live"}}}),
                )
                .await;
                let sub = read(&mut r).await;
                assert_eq!(sub["method"], "events.subscribe");
                assert_eq!(sub["params"]["session_id"], json!("s-live"));
                write(&mut w, json!({"jsonrpc":"2.0","id":sub["id"],"result":{}})).await;
                let status = read(&mut r).await;
                assert_eq!(status["method"], "status.get");
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":status["id"],"result":{"state":"working","seq":1,"session":{"session_id":"s-live"}}}),
                )
                .await;
            };
            tokio::select! {
                result = &mut idle => panic!("idle waiter finished before close: {result:?}"),
                () = server => {}
            }
            // Wait until the response was dispatched, then poll once more so wait_idle is
            // actually parked on `changed`, rather than still waiting for its status reply.
            tokio::time::timeout(Duration::from_secs(2), async {
                while !client.inner.state.lock().unwrap().pending.is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("status reply was not dispatched");
            std::future::poll_fn(|cx| {
                assert!(matches!(idle.as_mut().poll(cx), Poll::Pending));
                Poll::Ready(())
            })
            .await;

            let mut changed = client.inner.changed.subscribe();
            let before = *changed.borrow_and_update();
            let (reply, pending) = tokio::sync::oneshot::channel();
            client.inner.state.lock().unwrap().pending.insert(999, reply);
            let reader = client.inner.reader.lock().await.take().unwrap();
            reader.abort();
            assert!(reader.await.unwrap_err().is_cancelled());
            *client.inner.reader.lock().await = Some(tokio::task::spawn_local(async {
                panic!("injected reader panic");
            }));

            tokio::join!(client.close(), client.close());
            let error = tokio::time::timeout(Duration::from_millis(250), &mut idle)
                .await
                .expect("idle waiter was not notified")
                .unwrap_err();
            assert_eq!(error.code, "closed");
            tokio::time::timeout(Duration::from_millis(250), changed.changed())
                .await
                .expect("changed observer was not notified")
                .unwrap();
            assert_eq!(*changed.borrow_and_update(), before + 1);
            assert_eq!(pending.await.unwrap().unwrap_err().code, "closed");
            assert!(*client.inner.finished.borrow());
        })
        .await;
}

#[tokio::test]
async fn pending_limit_rejects_pressure_without_misrouting_other_replies() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (client, mut r, mut w) = pair().await;
            let calls = async {
                let mut tasks = tokio::task::JoinSet::new();
                for _ in 0..MAX_PENDING + 1 {
                    let client = client.clone();
                    tasks.spawn_local(async move { client.status(WaitOptions::default()).await });
                }
                let mut results = Vec::new();
                while let Some(result) = tasks.join_next().await {
                    results.push(result.unwrap());
                }
                results
            };
            let server = async {
                let mut requests = Vec::new();
                for _ in 0..MAX_PENDING {
                    requests.push(read(&mut r).await);
                }
                for request in requests.into_iter().rev() {
                    write(&mut w, json!({"jsonrpc":"2.0","id":request["id"],"result":{"state":"idle"}})).await;
                }
            };
            let (results, ()) = tokio::join!(calls, server);
            assert_eq!(results.iter().filter(|r| r.is_ok()).count(), MAX_PENDING);
            assert_eq!(results.into_iter().find_map(|r| r.err()).unwrap().code, "capacity");
            client.close().await;
        })
        .await;
}

#[tokio::test]
async fn malformed_remote_frame_closes_and_wakes_pending_calls() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (client, mut r, mut w) = pair().await;
            let call = client.status(WaitOptions { timeout: None, ..Default::default() });
            let server = async {
                let _ = read(&mut r).await;
                w.write_all(b"not-json\n").await.unwrap();
            };
            let (result, ()) = tokio::join!(call, server);
            assert_eq!(result.unwrap_err().code, "protocol_error");
            assert!(client.is_closed());
            client.close().await;
        })
        .await;
}

#[tokio::test]
async fn reused_message_ids_are_unknown_instead_of_reusing_an_old_receipt() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (client, mut r, mut w) = pair().await;
            for round in 0..2 {
                let send = client.send("message".into(), false, WaitOptions::default());
                let server = async {
                    let call = read(&mut r).await;
                    write(
                        &mut w,
                        json!({"jsonrpc":"2.0","id":call["id"],"result":{"message_id":"reused","status":"held"}}),
                    )
                    .await;
                };
                let (result, ()) = tokio::join!(send, server);
                if round == 0 {
                    assert!(result.is_ok());
                } else {
                    assert_eq!(result.unwrap_err().code, "outcome_unknown");
                }
            }
            client.close().await;
        })
        .await;
}

#[tokio::test]
async fn idle_cancel_and_stale_seq_cannot_satisfy_next_wait() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (client, mut r, mut w) = pair().await;
            let cancel = CancellationToken::new();
            let first = client.wait_idle(false, WaitOptions { timeout: None, cancel: cancel.clone() });
            let server = async {
                let st0 = read(&mut r).await;
                assert_eq!(st0["method"], "status.get");
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":st0["id"],"result":{"state":"working","seq":1,"session":{"session_id":"s-live"}}}),
                )
                .await;
                let sub = read(&mut r).await;
                assert_eq!(sub["method"], "events.subscribe");
                assert_eq!(sub["params"]["session_id"], json!("s-live"));
                write(&mut w, json!({"jsonrpc":"2.0","id":sub["id"],"result":{"mode":"live","session_epoch":"e1","seq":1}})).await;
                let status = read(&mut r).await;
                assert_eq!(status["method"], "status.get");
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":status["id"],"result":{"state":"working","seq":1,"session":{"session_id":"s-live"}}}),
                )
                .await;
                cancel.cancel();
            };
            let (cancelled, ()) = tokio::join!(first, server);
            assert_eq!(cancelled.unwrap_err().code, "cancelled");

            // Stale `seq: 2` idle arrives while idle, then a new turn begins at `seq: 3`.
            write(
                &mut w,
                json!({"jsonrpc":"2.0","method":"status/changed","params":{"state":"idle","seq":2,"session":{"session_id":"s-live"}}}),
            )
            .await;
            let next = client.wait_idle(false, WaitOptions::default());
            let server = async {
                let status = read(&mut r).await;
                assert_eq!(status["method"], "status.get");
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":status["id"],"result":{"state":"working","seq":3,"session":{"session_id":"s-live"}}}),
                )
                .await;
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","method":"event","params":{"session_id":"s-live","type":"status/changed","data":{"state":"idle","seq":4},"seq":4}}),
                )
                .await;
            };
            let (next, ()) = tokio::join!(next, server);
            assert_eq!(next.unwrap()["seq"], 4);

            // `current_or_next = true` returns the idle snapshot immediately without waiting for another event.
            let immediate = client.wait_idle(true, WaitOptions::default());
            let server = async {
                let status = read(&mut r).await;
                assert_eq!(status["method"], "status.get");
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":status["id"],"result":{"state":"idle","seq":4,"session":{"session_id":"s-live"}}}),
                )
                .await;
            };
            let (immediate, ()) = tokio::join!(immediate, server);
            assert_eq!(immediate.unwrap()["seq"], 4);
            client.close().await;
        })
        .await;
}

#[tokio::test]
async fn dropped_idle_future_and_remote_disconnect_wake_and_close_client() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (client, mut r, mut w) = pair().await;
            let dropped = tokio::time::timeout(
                Duration::from_millis(20),
                client.wait_idle(false, WaitOptions { timeout: None, ..Default::default() }),
            );
            let server = async {
                let st0 = read(&mut r).await;
                assert_eq!(st0["method"], "status.get");
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":st0["id"],"result":{"state":"working","seq":1,"session":{"session_id":"s-live"}}}),
                )
                .await;
                let sub = read(&mut r).await;
                assert_eq!(sub["method"], "events.subscribe");
                assert_eq!(sub["params"]["session_id"], json!("s-live"));
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":sub["id"],"result":{"mode":"live","session_epoch":"e1","seq":1}}),
                )
                .await;
                let status = read(&mut r).await;
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":status["id"],"result":{"state":"working","seq":1,"session":{"session_id":"s-live"}}}),
                )
                .await;
            };
            let (dropped, ()) = tokio::join!(dropped, server);
            assert!(dropped.is_err());

            // Dropping the server write half during a wait terminates `wait_idle` cleanly.
            let waiting = client.wait_idle(false, WaitOptions::default());
            let disconnect = async {
                let status = read(&mut r).await;
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":status["id"],"result":{"state":"working","seq":2,"session":{"session_id":"s-live"}}}),
                )
                .await;
                drop(w);
            };
            let (res, ()) = tokio::join!(waiting, disconnect);
            assert!(res.is_err());
            client.close().await;
            assert!(client.is_closed());
        })
        .await;
}

#[tokio::test]
async fn wait_idle_resubscribes_and_ignores_old_session_notifications_after_remote_session_switch() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (client, mut r, mut w) = pair().await;

            // 1. Subscribe and settle on session `s1` (ending at high seq=10).
            let first = client.wait_idle(false, WaitOptions::default());
            let server1 = async {
                let st0 = read(&mut r).await;
                assert_eq!(st0["method"], "status.get");
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":st0["id"],"result":{"state":"working","seq":9,"session":{"session_id":"s1"}}}),
                )
                .await;
                let sub1 = read(&mut r).await;
                assert_eq!(sub1["method"], "events.subscribe");
                assert_eq!(sub1["params"]["session_id"], json!("s1"));
                write(&mut w, json!({"jsonrpc":"2.0","id":sub1["id"],"result":{"mode":"live","session_epoch":"e1","seq":10}})).await;
                let st1 = read(&mut r).await;
                assert_eq!(st1["method"], "status.get");
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":st1["id"],"result":{"state":"working","seq":9,"session":{"session_id":"s1"}}}),
                )
                .await;
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","method":"event","params":{"session_id":"s1","type":"status/changed","data":{"state":"idle","seq":10,"session":{"session_id":"s1"}},"seq":10}}),
                )
                .await;
            };
            let (first, ()) = tokio::join!(first, server1);
            assert_eq!(first.unwrap()["seq"], 10);

            // 2. Remote switches to session `s2` where seq restarts at 1.
            //    `wait_idle` must re-subscribe to `s2`, drop stale `s1` idle (even with higher seq=11),
            //    and resolve on `s2`'s idle at seq=2.
            let second = client.wait_idle(false, WaitOptions::default());
            let server2 = async {
                let st0 = read(&mut r).await;
                assert_eq!(st0["method"], "status.get");
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":st0["id"],"result":{"state":"working","seq":1,"session":{"session_id":"s2"}}}),
                )
                .await;
                let sub2 = read(&mut r).await;
                assert_eq!(sub2["method"], "events.subscribe");
                assert_eq!(sub2["params"]["session_id"], json!("s2"));
                write(&mut w, json!({"jsonrpc":"2.0","id":sub2["id"],"result":{"mode":"live","session_epoch":"e2","seq":1}})).await;
                let st1 = read(&mut r).await;
                assert_eq!(st1["method"], "status.get");
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","id":st1["id"],"result":{"state":"working","seq":1,"session":{"session_id":"s2"}}}),
                )
                .await;
                // Late notification from old session `s1` with higher seq=11 must be ignored.
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","method":"event","params":{"session_id":"s1","type":"status/changed","data":{"state":"idle","seq":11,"session":{"session_id":"s1"}},"seq":11}}),
                )
                .await;
                tokio::task::yield_now().await;
                // Real idle on new session `s2` with lower numeric seq=2 must resolve.
                write(
                    &mut w,
                    json!({"jsonrpc":"2.0","method":"event","params":{"session_id":"s2","type":"status/changed","data":{"state":"idle","seq":2,"session":{"session_id":"s2"}},"seq":2}}),
                )
                .await;
            };
            let (second, ()) = tokio::join!(second, server2);
            let res = second.unwrap();
            assert_eq!(res["seq"], 2, "must resolve with s2 idle, not stale s1 idle: {res}");
            assert_eq!(res["session"]["session_id"], "s2", "{res}");
            client.close().await;
        })
        .await;
}

#[tokio::test]
async fn queue_only_requires_advertised_delivery_capability() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (mut client, mut r, mut w) = pair().await;
            let err = client.send("queued".into(), true, WaitOptions::default()).await.unwrap_err();
            assert_eq!(err.code, "unsupported");
            client.info["delivery_modes"] = json!(["trigger_turn", "queue_only"]);
            let send = client.send("queued".into(), true, WaitOptions::default());
            let server = async {
                let call = read(&mut r).await;
                assert!(call["params"].get("notifyWhenIdle").is_none(), "notifyWhenIdle must not be sent: {call}");
                assert_eq!(call["params"]["queue_only"], true);
                write(&mut w, json!({"jsonrpc":"2.0","id":call["id"],"result":{"message_id":"q1","status":"held"}}))
                    .await;
            };
            let (result, ()) = tokio::join!(send, server);
            assert_eq!(result.unwrap()["status"], "held");
            client.close().await;
        })
        .await;
}
