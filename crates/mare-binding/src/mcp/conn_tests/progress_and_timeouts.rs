//! Progress token routing, timeouts, overruns, and initialization cleanup.

use super::*;

#[tokio::test(start_paused = true)]
async fn matching_progress_resets_only_its_call_idle_timeout() {
    use tokio::io::AsyncWriteExt;
    let (inner, service, mut writer, mut reader, mut seen) = progress_test_client();
    let (task, request) = start_progress_test_call(&inner, &service, &mut reader).await;
    tokio::time::advance(Duration::from_millis(60)).await;
    send_test_progress(&mut writer, &mut seen, request["params"]["_meta"]["progressToken"].clone()).await;
    tokio::time::advance(Duration::from_millis(60)).await;
    assert!(!task.is_finished(), "matching progress did not extend the idle deadline");
    let response =
        serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":{"content":[{"type":"text","text":"ok"}]}});
    writer.write_all(format!("{response}\n").as_bytes()).await.unwrap();
    assert!(task.await.unwrap().is_ok());
    assert_eq!(inner.progress.receiver_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn human_callback_pauses_idle_and_total_then_restores_remaining_budget() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (inner, service, mut writer, mut reader, _seen) = progress_test_client();
    let (task, _request) = start_progress_test_call(&inner, &service, &mut reader).await;
    tokio::time::advance(Duration::from_millis(40)).await;
    let request = serde_json::json!({"jsonrpc":"2.0", "id":900, "method":"elicitation/create",
        "params":{"mode":"form", "message":"name", "requestedSchema":{"type":"object","properties":{}}}});
    writer.write_all(format!("{request}\n").as_bytes()).await.unwrap();
    let id = match inner.rx.lock().await.recv().await.unwrap() {
        Event::Elicit { id, .. } => id,
        other => panic!("unexpected event: {other:?}"),
    };
    // Longer than both the 100ms idle and 1000ms total budgets.
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!task.is_finished());
    inner.pending.lock().unwrap().remove(&id).unwrap().send(serde_json::json!({"action":"cancel"})).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let answer: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(answer["result"]["action"], "cancel");
    tokio::time::advance(Duration::from_millis(59)).await;
    tokio::task::yield_now().await;
    assert!(!task.is_finished(), "remaining idle budget was lost");
    tokio::time::advance(Duration::from_millis(2)).await;
    assert!(task.await.unwrap().unwrap_err().contains("timed out after 100ms"));
    assert_eq!(inner.progress.receiver_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn cancelled_elicitation_context_clears_pending_and_resumes_active_time() {
    use tokio::io::AsyncWriteExt;
    let (inner, service, mut writer, mut reader, _seen) = progress_test_client();
    let (task, _request) = start_progress_test_call(&inner, &service, &mut reader).await;
    let request = serde_json::json!({"jsonrpc":"2.0", "id":900, "method":"elicitation/create",
        "params":{"mode":"form", "message":"name", "requestedSchema":{"type":"object","properties":{}}}});
    writer.write_all(format!("{request}\n").as_bytes()).await.unwrap();
    let (id, cancel) = match inner.rx.lock().await.recv().await.unwrap() {
        Event::Elicit { id, cancel, .. } => (id, cancel),
        other => panic!("unexpected event: {other:?}"),
    };
    assert!(inner.pending.lock().unwrap().contains_key(&id));
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!task.is_finished());
    let notification =
        serde_json::json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":900}});
    writer.write_all(format!("{notification}\n").as_bytes()).await.unwrap();
    // rmcp suppresses a response for a cancelled incoming request. Its token still
    // closes the callback and releases the clock rather than waiting for an answer.
    let resumed_at = tokio::time::Instant::now();
    cancel.cancelled().await;
    tokio::time::advance(Duration::from_millis(101)).await;
    assert!(task.await.unwrap().unwrap_err().contains("timed out after 100ms"));
    assert!(resumed_at.elapsed() < Duration::from_secs(1), "cancelled request retained the paused clock");
    assert!(inner.pending.lock().unwrap().is_empty(), "cancelled context retained its answer sender");
}

#[tokio::test(start_paused = true)]
async fn unrelated_progress_does_not_extend_idle_timeout() {
    use tokio::io::AsyncBufReadExt;
    let (inner, service, mut writer, mut reader, mut seen) = progress_test_client();
    let (task, _request) = start_progress_test_call(&inner, &service, &mut reader).await;
    tokio::time::advance(Duration::from_millis(60)).await;
    send_test_progress(&mut writer, &mut seen, serde_json::json!("unrelated")).await;
    tokio::time::advance(Duration::from_millis(41)).await;
    assert!(task.await.unwrap().unwrap_err().contains("timed out"));
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let cancel: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(cancel["method"], "notifications/cancelled");
    assert_eq!(inner.progress.receiver_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn continuous_progress_cannot_extend_total_timeout() {
    let (inner, service, mut writer, mut reader, mut seen) = progress_test_client();
    let (task, request) = start_progress_test_call(&inner, &service, &mut reader).await;
    let token = request["params"]["_meta"]["progressToken"].clone();
    for _ in 0..11 {
        tokio::time::advance(Duration::from_millis(90)).await;
        send_test_progress(&mut writer, &mut seen, token.clone()).await;
    }
    tokio::time::advance(Duration::from_millis(11)).await;
    assert!(task.await.unwrap().unwrap_err().contains("total timeout"));
    assert_eq!(inner.progress.receiver_count(), 0);
}

#[tokio::test]
async fn canceled_tool_call_releases_progress_receiver_and_notifies_server() {
    use tokio::io::AsyncBufReadExt;
    let (inner, service, _writer, mut reader, _seen) = progress_test_client();
    let (task, _request) = start_progress_test_call(&inner, &service, &mut reader).await;
    assert_eq!(inner.progress.receiver_count(), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await.unwrap().unwrap();
    let cancel: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(cancel["method"], "notifications/cancelled");
    assert_eq!(inner.progress.receiver_count(), 0);
}

#[tokio::test]
async fn progress_overrun_fails_and_releases_receiver() {
    use tokio::io::AsyncBufReadExt;
    let (inner, service, _writer, mut reader, _seen) = progress_test_client();
    let peer = service.peer().clone();
    let sender = inner.clone();
    let (checked_tx, checked_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        call_tool_with_before_submit(
            peer,
            "echo".into(),
            serde_json::Map::new(),
            Duration::from_secs(3),
            &sender,
            Some(0),
            None,
            async move {
                let _ = checked_tx.send(());
                let _ = release_rx.await;
            },
        )
        .await
    });
    checked_rx.await.unwrap();
    for n in 0..=PROGRESS_CAPACITY {
        let _ = inner.progress.send(ProgressUpdate {
            token: ProgressToken(rmcp::model::NumberOrString::Number(n as i64)),
            progress: n as f64,
            total: None,
            message: None,
        });
    }
    release_tx.send(()).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(task.await.unwrap().unwrap_err().contains("overrun"));
    line.clear();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await.unwrap().unwrap();
    let cancel: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(cancel["method"], "notifications/cancelled");
    assert_eq!(inner.progress.receiver_count(), 0);
}

/// Item 7 (self-written): a progress notification sent immediately before the tool response
/// (0ms gap) must not be dropped by `biased` select.
#[tokio::test]
async fn back_to_back_progress_before_response_is_not_dropped() {
    use tokio::io::AsyncBufReadExt;
    let (inner, service, mut writer, mut reader, _seen) = progress_test_client();
    let peer = service.peer().clone();
    let (ptx, mut prx) = tokio::sync::mpsc::channel(PROGRESS_CAPACITY);
    let sender = inner.clone();
    let task = tokio::spawn(async move {
        call_tool_with_progress(
            peer,
            "echo".into(),
            serde_json::Map::new(),
            Duration::from_secs(3),
            &sender,
            Some(0),
            Some(ptx),
        )
        .await
    });
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let req: serde_json::Value = serde_json::from_str(&line).unwrap();
    let id = req["id"].clone();
    let token = req["params"]["_meta"]["progressToken"].clone();
    // Send progress notification and response back-to-back with no delay.
    let notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": { "progressToken": token, "progress": 1, "total": 1, "message": "instant" }
    });
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": "done" }] }
    });
    use tokio::io::AsyncWriteExt;
    writer.write_all(format!("{notif}\n{resp}\n").as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    let res = task.await.unwrap().unwrap();
    assert_eq!(res["content"][0]["text"], "done");
    let mut received = Vec::new();
    while let Ok(p) = prx.try_recv() {
        received.push(p);
    }
    assert_eq!(received.len(), 1, "back-to-back progress notification must not be dropped");
    assert_eq!(received[0].message.as_deref(), Some("instant"));
}

#[tokio::test]
async fn delayed_progress_task_after_response_is_still_delivered_before_return() {
    use rmcp::service::serve_directly_with_ct;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let spec = Spec::parse(serde_json::json!({ "command": "unused" })).unwrap();
    let inner = Inner::new(spec, None);
    let (mut writer, client_read) = tokio::io::duplex(65536);
    let (client_write, remote_read) = tokio::io::duplex(65536);
    let mut reader = tokio::io::BufReader::new(remote_read);
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let (hold_tx, hold_rx) = tokio::sync::watch::channel(false);
    let (resp_tx, mut resp_rx) = mpsc::unbounded_channel();
    *inner.response_received.lock().unwrap() = Some(resp_tx);
    let mut handler = inner.handler();
    handler.progress_hold = Some((entered_tx, hold_rx));
    let tracked = crate::mcp::conn_handler::ProgressTrackingTransport::new(
        rmcp::transport::IntoTransport::into_transport((client_read, client_write)),
        handler.progress_tracker.clone(),
    );
    let service = serve_directly_with_ct::<RoleClient, _, _, _, _>(handler, tracked, None, CancellationToken::new());

    let peer = service.peer().clone();
    let (ptx, mut prx) = tokio::sync::mpsc::channel(PROGRESS_CAPACITY);
    let sender = inner.clone();
    let call_task = tokio::spawn(async move {
        call_tool_with_progress(
            peer,
            "echo".into(),
            serde_json::Map::new(),
            Duration::from_secs(3),
            &sender,
            Some(0),
            Some(ptx),
        )
        .await
    });

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let req: serde_json::Value = serde_json::from_str(&line).unwrap();
    let id = req["id"].clone();
    let token = req["params"]["_meta"]["progressToken"].clone();

    let notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": { "progressToken": token, "progress": 1, "total": 1, "message": "delayed" }
    });
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": "done" }] }
    });
    writer.write_all(format!("{notif}\n{resp}\n").as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    // Wait until the spawned on_progress task is paused AND call_tool has received the response
    // and entered the post-response completion barrier.
    entered_rx.recv().await.unwrap();
    resp_rx.recv().await.unwrap();
    assert!(!call_task.is_finished(), "call_tool must wait for in-flight progress task before returning");

    hold_tx.send(true).unwrap();
    let res = call_task.await.unwrap().unwrap();
    assert_eq!(res["content"][0]["text"], "done");
    let mut received = Vec::new();
    while let Ok(p) = prx.try_recv() {
        received.push(p);
    }
    assert_eq!(received.len(), 1, "delayed progress notification must be delivered before return");
    assert_eq!(received[0].message.as_deref(), Some("delayed"));
}

#[tokio::test]
async fn post_response_progress_overrun_returns_error_instead_of_silent_drop() {
    use rmcp::service::serve_directly_with_ct;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let spec = Spec::parse(serde_json::json!({ "command": "unused" })).unwrap();
    let inner = Inner::new(spec, None);
    let (mut writer, client_read) = tokio::io::duplex(65536);
    let (client_write, remote_read) = tokio::io::duplex(65536);
    let mut reader = tokio::io::BufReader::new(remote_read);
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let (hold_tx, hold_rx) = tokio::sync::watch::channel(false);
    let (resp_tx, mut resp_rx) = mpsc::unbounded_channel();
    *inner.response_received.lock().unwrap() = Some(resp_tx);
    let mut handler = inner.handler();
    handler.progress_hold = Some((entered_tx, hold_rx));
    let tracked = crate::mcp::conn_handler::ProgressTrackingTransport::new(
        rmcp::transport::IntoTransport::into_transport((client_read, client_write)),
        handler.progress_tracker.clone(),
    );
    let service = serve_directly_with_ct::<RoleClient, _, _, _, _>(handler, tracked, None, CancellationToken::new());

    let peer = service.peer().clone();
    let (ptx, _prx) = tokio::sync::mpsc::channel(PROGRESS_CAPACITY);
    let sender = inner.clone();
    let call_task = tokio::spawn(async move {
        call_tool_with_progress(
            peer,
            "echo".into(),
            serde_json::Map::new(),
            Duration::from_secs(3),
            &sender,
            Some(0),
            Some(ptx),
        )
        .await
    });

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let req: serde_json::Value = serde_json::from_str(&line).unwrap();
    let id = req["id"].clone();
    let token_val = req["params"]["_meta"]["progressToken"].clone();

    let notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": { "progressToken": token_val, "progress": 1, "total": 1, "message": "delayed" }
    });
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": "done" }] }
    });
    writer.write_all(format!("{notif}\n{resp}\n").as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    entered_rx.recv().await.unwrap();
    resp_rx.recv().await.unwrap();

    // Overflow the broadcast queue while call_tool is waiting at the post-response barrier.
    let other_token = rmcp::model::ProgressToken(rmcp::model::NumberOrString::String("other".into()));
    for i in 0..=PROGRESS_CAPACITY {
        let _ = inner.progress.send(ProgressUpdate {
            token: other_token.clone(),
            progress: i as f64,
            total: None,
            message: None,
        });
    }

    hold_tx.send(true).unwrap();
    let err = call_task.await.unwrap().unwrap_err();
    assert!(err.contains("overrun"), "expected overrun error, got: {err}");
}

#[tokio::test(start_paused = true)]
async fn post_response_progress_wait_respects_total_timeout() {
    use rmcp::service::serve_directly_with_ct;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let spec = Spec::parse(serde_json::json!({ "command": "unused" })).unwrap();
    let inner = Inner::new(spec, None);
    let (mut writer, client_read) = tokio::io::duplex(65536);
    let (client_write, remote_read) = tokio::io::duplex(65536);
    let mut reader = tokio::io::BufReader::new(remote_read);
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let (hold_tx, hold_rx) = tokio::sync::watch::channel(false);
    let (resp_tx, mut resp_rx) = mpsc::unbounded_channel();
    *inner.response_received.lock().unwrap() = Some(resp_tx);
    let mut handler = inner.handler();
    handler.progress_hold = Some((entered_tx, hold_rx));
    let tracked = crate::mcp::conn_handler::ProgressTrackingTransport::new(
        rmcp::transport::IntoTransport::into_transport((client_read, client_write)),
        handler.progress_tracker.clone(),
    );
    let service = serve_directly_with_ct::<RoleClient, _, _, _, _>(handler, tracked, None, CancellationToken::new());

    let peer = service.peer().clone();
    let (ptx, _prx) = tokio::sync::mpsc::channel(PROGRESS_CAPACITY);
    let sender = inner.clone();
    let timeout = Duration::from_millis(100);
    let call_task = tokio::spawn(async move {
        call_tool_with_progress(peer, "echo".into(), serde_json::Map::new(), timeout, &sender, Some(0), Some(ptx)).await
    });

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let req: serde_json::Value = serde_json::from_str(&line).unwrap();
    let id = req["id"].clone();
    let token = req["params"]["_meta"]["progressToken"].clone();

    let notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": { "progressToken": token, "progress": 1, "total": 1, "message": "stalled" }
    });
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": "done" }] }
    });
    writer.write_all(format!("{notif}\n{resp}\n").as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    entered_rx.recv().await.unwrap();
    resp_rx.recv().await.unwrap();

    // Advance past total_deadline (10 * timeout = 1000ms) while on_progress remains paused.
    tokio::time::advance(Duration::from_millis(1001)).await;
    let err = call_task.await.unwrap().unwrap_err();
    assert!(err.contains("exceeded its total timeout"), "expected total timeout error, got: {err}");
    let _ = hold_tx.send(true);
}

#[tokio::test]
async fn interleaved_progress_wait_before_waits_for_pre_response_notification_even_if_later_completes_first() {
    use rmcp::service::serve_directly_with_ct;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let spec = Spec::parse(serde_json::json!({ "command": "unused" })).unwrap();
    let inner = Inner::new(spec, None);
    let (mut writer, client_read) = tokio::io::duplex(65536);
    let (client_write, remote_read) = tokio::io::duplex(65536);
    let mut reader = tokio::io::BufReader::new(remote_read);
    let (gate_tx, mut gate_rx) = mpsc::unbounded_channel();
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let (resp_tx, mut resp_rx) = mpsc::unbounded_channel();
    *inner.response_received.lock().unwrap() = Some(resp_tx);
    let mut handler = inner.handler();
    handler.progress_hold_per_notif = Some(gate_tx);
    handler.progress_seen = Some(seen_tx);
    let tracked = crate::mcp::conn_handler::ProgressTrackingTransport::new(
        rmcp::transport::IntoTransport::into_transport((client_read, client_write)),
        handler.progress_tracker.clone(),
    );
    let service = serve_directly_with_ct::<RoleClient, _, _, _, _>(handler, tracked, None, CancellationToken::new());

    let peer = service.peer().clone();
    let (ptx, mut prx) = tokio::sync::mpsc::channel(PROGRESS_CAPACITY);
    let sender = inner.clone();
    let call_task = tokio::spawn(async move {
        call_tool_with_progress(
            peer,
            "echo".into(),
            serde_json::Map::new(),
            Duration::from_secs(3),
            &sender,
            Some(0),
            Some(ptx),
        )
        .await
    });

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let req: serde_json::Value = serde_json::from_str(&line).unwrap();
    let id = req["id"].clone();
    let token = req["params"]["_meta"]["progressToken"].clone();

    // Send notification A before the response and hold it in on_progress.
    let notif_a = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": { "progressToken": token, "progress": 1, "total": 2, "message": "A" }
    });
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": "done" }] }
    });
    writer.write_all(format!("{notif_a}\n{resp}\n").as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    let (msg_a, release_a) = gate_rx.recv().await.unwrap();
    assert_eq!(msg_a.as_deref(), Some("A"));
    resp_rx.recv().await.unwrap();
    assert!(!call_task.is_finished(), "call_tool must wait while A is in-flight");

    // Send notification B (same token) after the response and let B complete before A.
    let notif_b = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": { "progressToken": token, "progress": 2, "total": 2, "message": "B" }
    });
    writer.write_all(format!("{notif_b}\n").as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    let (msg_b, release_b) = gate_rx.recv().await.unwrap();
    assert_eq!(msg_b.as_deref(), Some("B"));
    release_b.send(()).unwrap();
    seen_rx.recv().await.unwrap();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    assert!(
        !call_task.is_finished(),
        "call_tool must NOT return when post-response B finishes while pre-response A is still held"
    );

    release_a.send(()).unwrap();
    let res = call_task.await.unwrap().unwrap();
    assert_eq!(res["content"][0]["text"], "done");

    let mut msgs = Vec::new();
    while let Ok(p) = prx.try_recv() {
        if let Some(m) = p.message {
            msgs.push(m);
        }
    }
    assert!(msgs.contains(&"A".to_string()), "pre-response notification A must be delivered: {msgs:?}");
    assert!(msgs.contains(&"B".to_string()), "notification B must be delivered: {msgs:?}");
}

#[tokio::test]
async fn progress_tracker_reclaims_completed_tokens() {
    use rmcp::service::serve_directly_with_ct;
    use tokio::io::AsyncWriteExt;

    let spec = Spec::parse(serde_json::json!({ "command": "unused" })).unwrap();
    let inner = Inner::new(spec, None);
    let (mut writer, client_read) = tokio::io::duplex(65536);
    let (client_write, _remote_read) = tokio::io::duplex(65536);
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let mut handler = inner.handler();
    handler.progress_seen = Some(seen_tx);
    let tracker = handler.progress_tracker.clone();
    let tracked = crate::mcp::conn_handler::ProgressTrackingTransport::new(
        rmcp::transport::IntoTransport::into_transport((client_read, client_write)),
        tracker.clone(),
    );
    let _service = serve_directly_with_ct::<RoleClient, _, _, _, _>(handler, tracked, None, CancellationToken::new());

    let count = 200;
    for i in 0..count {
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": { "progressToken": format!("tok-{i}"), "progress": 1, "total": 1 }
        });
        writer.write_all(format!("{notif}\n").as_bytes()).await.unwrap();
    }
    writer.flush().await.unwrap();

    for _ in 0..count {
        seen_rx.recv().await.unwrap();
    }
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    assert_eq!(tracker.token_count(), 0, "completed progress tokens must be reclaimed from tracker");
}

#[tokio::test]
async fn initialize_dropped_progress_notifications_do_not_leak_tracker() {
    use rmcp::ServiceExt as _;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let spec = Spec::parse(serde_json::json!({ "command": "unused" })).unwrap();
    let inner = Inner::new(spec, None);
    let (mut writer, client_read) = tokio::io::duplex(65536);
    let (client_write, remote_read) = tokio::io::duplex(65536);
    let handler = inner.handler();
    let tracker = handler.progress_tracker.clone();
    let tracked = crate::mcp::conn_handler::ProgressTrackingTransport::new(
        rmcp::transport::IntoTransport::into_transport((client_read, client_write)),
        tracker.clone(),
    );

    let server = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(remote_read);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let req: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(req["method"], "initialize");

        send_progress_notifications(&mut writer, "init-tok", 5).await;

        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": req["id"],
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": { "name": "test-server", "version": "1.0.0" }
            }
        });
        writer.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
        writer.flush().await.unwrap();

        let mut initialized = String::new();
        reader.read_line(&mut initialized).await.unwrap();
        writer
    });

    let svc = handler.serve(tracked).await.expect("initialize should succeed");
    let _writer = server.await.unwrap();

    assert_eq!(tracker.token_count(), 0, "progress tokens sent during initialize must not leak");
    let _ = svc.cancel().await;
}
