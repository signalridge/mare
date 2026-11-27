use super::*;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

struct NoopByteStream;
impl mare_shared::wire_projection::ByteStreamProjector for NoopByteStream {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        chunk.to_vec()
    }
    fn finish(&mut self) -> Vec<u8> {
        Vec::new()
    }
}

#[tokio::test]
async fn headless_filter_can_select_a_later_event() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let kernel = Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
    let selected = Arc::new(AtomicBool::new(false));
    let filter = selected.clone();
    let (_tap, mut rx) = kernel.open_headless_cursor(Arc::new(move |_| {
        filter.load(Ordering::SeqCst).then_some(HeadlessSelection { selected: true, required: false })
    }));
    assert!(!kernel.tapped("probe/dynamic"));
    assert!(kernel.taps_dynamic(), "the bus must recheck a headless filter on each event");
    selected.store(true, Ordering::SeqCst);
    assert!(kernel.tapped("probe/dynamic"));
    kernel.tap_event("probe/dynamic", &mlua::MultiValue::new()).await;
    let event = rx.try_recv().expect("the later event was selected");
    let HeadlessRecord::Event(event) = event.record else { panic!("expected event") };
    assert_eq!(event.name, "probe/dynamic");
}

fn poll_once<F: std::future::Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[tokio::test]
async fn waiting_tap_event_keeps_its_session_when_identity_changes() {
    use mlua::LuaSerdeExt;

    let kernel = Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
    let log = &kernel.0.event_log;
    log.set_limits(EventLogLimits { capacity: 1, block_limit: Duration::from_secs(5), ..EventLogLimits::default() });
    log.set_session(Some("old".into()));
    let (_, old_epoch) = log.session_identity();
    let (handle, mut cursor) =
        kernel.open_headless_cursor(Arc::new(|_| Some(HeadlessSelection { required: true, selected: true })));
    let first = mlua::MultiValue::from_vec(vec![kernel.lua().to_value(&serde_json::json!({"n": 1})).unwrap()]);
    let pending = mlua::MultiValue::from_vec(vec![kernel.lua().to_value(&serde_json::json!({"n": 2})).unwrap()]);
    assert_eq!(kernel.tap_event("agent/start", &first).await.seq, Some(1));
    let mut waiting = Box::pin(kernel.tap_event("agent/turn-end", &pending));
    assert!(poll_once(waiting.as_mut()).is_pending(), "the fixed cursor must pin the full ring");
    let mut waiting_again = Box::pin(kernel.tap_event("agent/turn-end", &pending));
    assert!(poll_once(waiting_again.as_mut()).is_pending(), "a second producer must wait behind the first");
    let new_epoch = log.switch_session(Some("new".into()));
    let first_record = cursor.try_recv().expect("the pinned event");
    assert!(matches!(first_record.record, HeadlessRecord::Event(ev) if ev.session_epoch == old_epoch));
    let envelope = waiting.await;
    let second = cursor.try_recv().expect("the waiting event must be delivered");
    let HeadlessRecord::Event(event) = second.record else { panic!("expected a waiting event") };
    assert_eq!(envelope.session_epoch, old_epoch);
    assert_eq!(event.session_epoch, old_epoch, "the old payload must not acquire the new session");
    assert_eq!(event.session_id.as_deref(), Some("old"));
    assert_eq!(event.seq, 2);
    assert_ne!(event.session_epoch, new_epoch);
    drop((handle, cursor));
    // Evict every old-session record before the second waiter obtains the admission lock.
    let new_event = kernel.tap_event("status/changed", &first).await;
    assert_eq!(new_event.session_epoch, new_epoch);
    assert_eq!(new_event.seq, Some(1), "the old event must not advance the new session sequence");
    let next_envelope = waiting_again.await;
    let event = log.lock().entries.back().expect("the second waiting event must be delivered").to_tap_event();
    assert_eq!(
        (next_envelope.session_epoch.as_str(), event.session_epoch.as_str()),
        (old_epoch.as_str(), old_epoch.as_str())
    );
    assert_eq!(event.seq, 3, "waiting producers must keep contiguous old-session sequence numbers");
}

/// Pending retained instances stay keyed by id until completion.
#[tokio::test]
async fn keyed_retained_events_survive_independently_and_clear_by_key() {
    use mlua::LuaSerdeExt;
    let kernel = Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
    let contracts: mlua::Table = kernel
        .lua()
        .load(
            r#"return { widget = { methods = {}, events = {
                ['activity-started'] = {
                  doc = 'Activity started', expose = 'both', delivery = 'lossy', ignorable = false,
                  retain = { key = 'item_id', cleared_by = 'widget/activity-completed' },
                  data = { type = 'object', properties = { item_id = { type = 'string' } }, required = { 'item_id' } },
                  example = { item_id = 'a' },
                },
                ['activity-completed'] = {
                  doc = 'Activity completed', expose = 'lua', delivery = 'lossy', ignorable = true,
                  data = { type = 'object', properties = { item_id = { type = 'string' } }, required = { 'item_id' } },
                  example = { item_id = 'a' },
                },
              } } }"#,
        )
        .eval()
        .unwrap();
    kernel.install_service_contracts(contracts).unwrap();
    let log = &kernel.0.event_log;
    log.set_limits(EventLogLimits { capacity: 1, ..EventLogLimits::default() });
    log.set_session(Some("session".into()));
    let (handle, mut cursor) =
        kernel.open_headless_cursor(Arc::new(|_| Some(HeadlessSelection { required: true, selected: true })));
    log.admit_json("turn/started", serde_json::json!({"turn_id": "t", "trigger": "prompt"})).await.unwrap();
    log.admit_json("widget/activity-started", serde_json::json!({"item_id": "a"})).await.unwrap();
    log.admit_json("widget/activity-started", serde_json::json!({"item_id": "b"})).await.unwrap();
    let (_, _, before) =
        log.subscribe_cursor_with_retained(None, None, false, Arc::new(|_| Some(true)), None, &|_| true);
    let active = before["widget/activity-started"].as_array().expect("keyed retention is an array of instances");
    assert_eq!(active.len(), 2, "both active instances must appear in a baseline");
    assert!(cursor.try_recv().is_some(), "consume the event that pins the ring");
    let mut delivered = Vec::new();
    for _ in 0..2 {
        handle.flush_lagged().await.unwrap();
        let record = cursor.try_recv().expect("a retained instance must be flushed");
        let HeadlessRecord::Event(event) = record.record else { panic!("expected a retained event") };
        delivered.push(event.payload["item_id"].as_str().unwrap().to_string());
    }
    delivered.sort();
    assert_eq!(delivered, ["a", "b"], "lossy coalescing must preserve each active key");
    drop((handle, cursor));
    let complete =
        mlua::MultiValue::from_vec(vec![kernel.lua().to_value(&serde_json::json!({"item_id": "a"})).unwrap()]);
    kernel.tap_event("widget/activity-completed", &complete).await;
    let (_, snapshot) = kernel
        .subscribe_with_baseline(
            1,
            None,
            None,
            false,
            Arc::new(|_| Some(true)),
            None,
            &|_| true,
            Default::default(),
            true,
            None,
        )
        .unwrap();
    let snapshot = snapshot.expect("reconnecting without a cursor requests a baseline");
    let active = snapshot.retained["events"]["widget/activity-started"].as_array().unwrap();
    assert_eq!(active, &[serde_json::json!({"item_id": "b"})]);
}

#[tokio::test]
async fn single_log_assigns_contiguous_seq_and_rotates_epoch_on_session_switch() {
    let log = EventLog::new();
    let mut all = log.open_cursor(Arc::new(|_| Some(true)));
    let mut only_end = log.open_cursor(Arc::new(|name| Some(name == "turn/completed")));

    log.set_session(Some("sess-1".into()));
    log.admit_json("turn/started", serde_json::json!({"turn_id": "t-1", "trigger": "prompt"})).await.unwrap();
    log.admit_json("turn/completed", serde_json::json!({"turn_id": "t-1", "status": "completed", "usage": {}}))
        .await
        .unwrap();

    let Some(CursorRecord::Event { event: e1, .. }) = all.try_recv().unwrap() else { panic!() };
    let Some(CursorRecord::Event { event: e2, .. }) = all.try_recv().unwrap() else { panic!() };
    assert_eq!((e1.seq, e2.seq), (1, 2));
    assert_eq!(e1.session_id.as_deref(), Some("sess-1"));
    assert_eq!(e1.turn_id.as_deref(), Some("t-1"));
    assert_eq!(e2.turn_id.as_deref(), Some("t-1"));
    assert!(
        e1.session_epoch.starts_with("sess-1#") && e1.session_epoch.ends_with(":1"),
        "expected nonce-scoped initial epoch, got {}",
        e1.session_epoch
    );

    let Some(CursorRecord::Event { selected: s1, .. }) = only_end.try_recv().unwrap() else { panic!() };
    let Some(CursorRecord::Event { event: s2, selected: sel2 }) = only_end.try_recv().unwrap() else { panic!() };
    assert!(!s1 && sel2);
    assert_eq!(s2.seq, 2);

    log.switch_session(Some("sess-2".into()));
    log.admit_json("session/changed", serde_json::json!({"session_id": "sess-2", "reason": "new"})).await.unwrap();
    let Some(CursorRecord::Event { event: e3, .. }) = all.try_recv().unwrap() else { panic!() };
    assert_ne!(e3.session_epoch, e1.session_epoch);
    assert_eq!(e3.seq, 1);
    assert_eq!(e3.session_id.as_deref(), Some("sess-2"));

    let log_b = EventLog::new();
    log_b.set_session(Some("sess-1".into()));
    assert_ne!(log_b.session_identity().1, e1.session_epoch);
}

#[tokio::test]
async fn dropped_records_keep_the_identity_of_each_session_across_rotation() {
    let log = EventLog::new();
    let old_epoch = log.switch_session(Some("old".into()));
    let old_identity = log.emission_identity();
    let mut cursor = log.open_cursor(Arc::new(|_| Some(true)));
    assert!(
        log.admit_json_scoped("probe/drop", serde_json::json!({}), Some(mare_protocol::EventScope::Turn))
            .await
            .is_err()
    );
    let new_epoch = log.switch_session(Some("new".into()));
    assert!(
        log.admit_json_scoped("probe/drop", serde_json::json!({}), Some(mare_protocol::EventScope::Turn))
            .await
            .is_err()
    );
    assert!(
        log.admit_json_scoped_seq(
            "probe/drop",
            serde_json::json!({}),
            Some(mare_protocol::EventScope::Turn),
            Some(old_identity)
        )
        .await
        .is_err()
    );
    let old = cursor.try_recv().unwrap().expect("old drop");
    let new = cursor.try_recv().unwrap().expect("new drop");
    let delayed = cursor.try_recv().unwrap().expect("delayed old drop");
    assert!(
        matches!(&old, CursorRecord::Dropped { count: 1, session_id: Some(id), session_epoch } if id == "old" && session_epoch == &old_epoch),
        "{old:?}"
    );
    assert!(
        matches!(&new, CursorRecord::Dropped { count: 1, session_id: Some(id), session_epoch } if id == "new" && session_epoch == &new_epoch),
        "{new:?}"
    );
    assert!(
        matches!(&delayed, CursorRecord::Dropped { count: 1, session_id: Some(id), session_epoch } if id == "old" && session_epoch == &old_epoch),
        "{delayed:?}"
    );
    assert!(cursor.try_recv().unwrap().is_none());
}

#[tokio::test]
async fn headless_dropped_counts_only_events_selected_for_that_stream() {
    let log = EventLog::with_limits(EventLogLimits { capacity: 1, ..EventLogLimits::default() });
    let (_handle, mut headless) = log.open_headless_cursor(Arc::new(|name| match name {
        "agent/start" => Some(HeadlessSelection { selected: true, required: true }),
        "agent/notice" => Some(HeadlessSelection { selected: true, required: false }),
        "probe/tracked" => Some(HeadlessSelection { selected: false, required: false }),
        _ => None,
    }));
    let mut hooks = log.open_cursor(Arc::new(|name| matches!(name, "hook/start" | "hook/end").then_some(true)));

    // The required event pins the ring while each consumer sees a different set of lossy events.
    log.admit_json("agent/start", serde_json::json!({"turn_id": "t1"})).await.unwrap();
    for name in ["hook/start", "hook/end", "probe/tracked", "agent/notice", "agent/notice"] {
        log.admit_json(name, serde_json::json!({})).await.unwrap();
    }

    assert!(matches!(headless.try_recv().unwrap().record, HeadlessRecord::Event(ev) if ev.name == "agent/start"));
    let dropped = headless.try_recv().unwrap().record;
    assert!(matches!(&dropped, HeadlessRecord::Dropped { count: 2, .. }), "headless drops: {dropped:?}");
    assert!(headless.try_recv().is_none());
    assert!(matches!(hooks.try_recv().unwrap(), Some(CursorRecord::Dropped { count: 2, .. })));
}

#[tokio::test]
async fn lossy_retain_coalesces_while_blocked_and_unretained_counts_dropped() {
    let log = EventLog::with_limits(EventLogLimits {
        capacity: 2,
        max_bytes: 1024 * 1024,
        max_event_bytes: 1024 * 1024,
        block_limit: Duration::from_secs(5),
    });
    let (handle, mut hcursor) =
        log.open_headless_cursor(Arc::new(|_| Some(HeadlessSelection { required: false, selected: true })));

    log.admit_json("turn/started", serde_json::json!({"turn_id": "t1", "trigger": "prompt"})).await.unwrap();
    log.admit_json("step/started", serde_json::json!({"turn_id": "t1", "step_id": "s1", "turn": 1})).await.unwrap();
    log.admit_json("host/notice", serde_json::json!({"level": "info", "text": "n1"})).await.unwrap();
    log.admit_json("host/notice", serde_json::json!({"level": "info", "text": "n2"})).await.unwrap();
    log.admit_json("status/changed", serde_json::json!({"state": "working", "message": "1", "session": {}, "seq": 1}))
        .await
        .unwrap();
    log.admit_json("status/changed", serde_json::json!({"state": "idle", "message": "2", "session": {}, "seq": 2}))
        .await
        .unwrap();

    let mut req_fut = Box::pin(
        log.admit_json("turn/completed", serde_json::json!({"turn_id": "t1", "status": "completed", "usage": {}})),
    );
    assert!(poll_once(req_fut.as_mut()).is_pending(), "required event must wait when fixed cursor stalls");

    // While Required waits, a concurrent Lossy event must yield and count Dropped instead of stealing the slot.
    log.admit_json("host/notice", serde_json::json!({"level": "info", "text": "yield-to-req"})).await.unwrap();

    assert!(matches!(hcursor.try_recv().unwrap().record, HeadlessRecord::Event(ev) if ev.name == "turn/started"));
    req_fut.await.unwrap();
    assert!(matches!(hcursor.try_recv().unwrap().record, HeadlessRecord::Event(ev) if ev.name == "step/started"));
    assert!(matches!(hcursor.try_recv().unwrap().record, HeadlessRecord::Dropped { count: 3, .. }));
    handle.flush_lagged().await.unwrap();
    assert!(matches!(hcursor.try_recv().unwrap().record, HeadlessRecord::Event(ev) if ev.name == "turn/completed"));
    let last = hcursor.try_recv().unwrap();
    assert!(
        matches!(last.record, HeadlessRecord::Event(ev) if ev.name == "status/changed" && ev.payload["state"] == "idle")
    );
}

#[tokio::test]
async fn required_event_larger_than_log_is_dropped_without_waiting_for_pinned_cursor() {
    let log = EventLog::with_limits(EventLogLimits {
        capacity: 2,
        max_bytes: 300,
        max_event_bytes: 2000,
        block_limit: Duration::from_secs(5),
    });
    let (handle, _cursor) =
        log.open_headless_cursor(Arc::new(|_| Some(HeadlessSelection { required: true, selected: true })));
    log.admit_json("agent/turn-end", serde_json::json!({ "n": 1 })).await.unwrap();
    let mut oversized = Box::pin(log.admit_json("agent/turn-end", serde_json::json!({ "text": "x".repeat(500) })));
    assert!(poll_once(oversized.as_mut()).is_ready(), "an event larger than the log must never block");
    assert!(handle.failed().is_none(), "an oversized event must not fail a healthy consumer");
}

#[tokio::test]
async fn waiting_required_event_rechecks_size_when_projection_revision_changes() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let log = EventLog::with_limits(EventLogLimits {
        capacity: 1,
        max_bytes: 300,
        max_event_bytes: 2000,
        block_limit: Duration::from_secs(5),
    });
    let revision = Arc::new(AtomicU64::new(1));
    let for_projection = revision.clone();
    let for_revision = revision.clone();
    log.set_wire_projector(
        mare_shared::wire_projection::WireProjector::new(
            Arc::new(AtomicBool::new(true)),
            Arc::new(move |value| {
                if for_projection.load(Ordering::SeqCst) > 1
                    && let Some(Value::String(text)) = value.get_mut("text")
                {
                    *text = text.replace("LATER", &"x".repeat(500));
                }
            }),
            Arc::new(str::to_owned),
            Arc::new(<[u8]>::to_vec),
            Arc::new(|| Box::new(NoopByteStream)),
        )
        .with_revision(Arc::new(move || for_revision.load(Ordering::SeqCst))),
    );
    let (handle, _cursor) =
        log.open_headless_cursor(Arc::new(|_| Some(HeadlessSelection { required: true, selected: true })));
    log.admit_json("agent/turn-end", serde_json::json!({ "n": 1 })).await.unwrap();
    let mut waiting = Box::pin(log.admit_json("agent/turn-end", serde_json::json!({ "text": "LATER" })));
    assert!(poll_once(waiting.as_mut()).is_pending());
    revision.store(2, Ordering::SeqCst);
    log.inner.space_wake.notify_waiters();
    assert!(poll_once(waiting.as_mut()).is_ready(), "new projected size must be checked before failing a consumer");
    assert!(handle.failed().is_none());
}

#[tokio::test]
async fn stalled_fixed_cursor_fails_only_itself_while_ordinary_cursor_lags_and_prunes_epochs() {
    let log = EventLog::with_limits(EventLogLimits {
        capacity: 2,
        max_bytes: 1024 * 1024,
        max_event_bytes: 1024 * 1024,
        block_limit: Duration::from_millis(5),
    });
    let (handle, mut hcursor) =
        log.open_headless_cursor(Arc::new(|_| Some(HeadlessSelection { required: true, selected: true })));
    let mut ord = log.open_cursor(Arc::new(|_| Some(true)));

    log.admit_json(
        "step/completed",
        serde_json::json!({"turn_id": "t1", "step_id": "s1", "status": "completed", "n": 1}),
    )
    .await
    .unwrap();
    log.admit_json(
        "step/completed",
        serde_json::json!({"turn_id": "t1", "step_id": "s2", "status": "completed", "n": 2}),
    )
    .await
    .unwrap();
    log.admit_json(
        "step/completed",
        serde_json::json!({"turn_id": "t1", "step_id": "s3", "status": "completed", "n": 3}),
    )
    .await
    .unwrap();

    assert!(handle.failed().is_some());
    assert!(hcursor.try_recv().is_none());

    log.admit_json(
        "step/completed",
        serde_json::json!({"turn_id": "t1", "step_id": "s4", "status": "completed", "n": 4}),
    )
    .await
    .unwrap();
    assert!(ord.failure().borrow().is_none());
    let lag = ord.try_recv().unwrap().unwrap();
    assert!(matches!(lag, CursorRecord::Lagged { from_seq: 1, to_seq: 2 }), "got {lag:?}");
    let next = ord.try_recv().unwrap().unwrap();
    assert!(matches!(next, CursorRecord::Event { event, .. } if event.seq == 3));
    let last = ord.try_recv().unwrap().unwrap();
    assert!(matches!(last, CursorRecord::Event { event, .. } if event.seq == 4));

    // Rotating away delivers remaining s2 events cleanly with sequence reset to 1.
    log.switch_session(Some("s2".into()));
    log.admit_json("agent/turn-end", serde_json::json!({"n": 5})).await.unwrap();
    log.admit_json(
        "step/completed",
        serde_json::json!({"turn_id": "t1", "step_id": "s6", "status": "completed", "n": 6}),
    )
    .await
    .unwrap();
    let Some(CursorRecord::Event { event: s2_first, .. }) = ord.try_recv().unwrap() else { panic!() };
    let Some(CursorRecord::Event { event: s2_second, .. }) = ord.try_recv().unwrap() else { panic!() };
    assert_eq!((s2_first.seq, s2_second.seq), (1, 2));
    assert!(ord.try_recv().unwrap().is_none());
}

#[tokio::test]
async fn subscribe_cursor_handles_evicted_boundary_and_tail_on_switch() {
    let log = EventLog::with_limits(EventLogLimits {
        capacity: 3,
        max_bytes: 1024 * 1024,
        max_event_bytes: 1024 * 1024,
        block_limit: Duration::from_secs(5),
    });
    log.switch_session(Some("s1".into()));
    let (_, epoch) = log.session_identity();
    for n in 1..=5 {
        log.admit_json(
            "step/completed",
            serde_json::json!({"turn_id": "t1", "step_id": format!("s{n}"), "status": "completed", "n": n}),
        )
        .await
        .unwrap();
    }
    // Ring holds seq 3..=5 (1 and 2 evicted): last_seq = 2 -> replay from 3; last_seq = 1 -> baseline.
    let mut at_boundary = log.subscribe_cursor(Some(&epoch), Some(2), false, Arc::new(|_| Some(true)), None);
    assert_eq!(at_boundary.mode, mare_protocol::SubscribeMode::Replay);
    assert_eq!(at_boundary.seq, 3);
    let Some(CursorRecord::Event { event: first, .. }) = at_boundary.cursor.try_recv().unwrap() else { panic!() };
    assert_eq!(first.seq, 3);

    let mut evicted = log.subscribe_cursor(Some(&epoch), Some(1), false, Arc::new(|_| Some(true)), None);
    assert_eq!(evicted.mode, mare_protocol::SubscribeMode::Baseline);
    assert_eq!(evicted.seq, 6);
    assert!(
        evicted.cursor.try_recv().unwrap().is_none(),
        "baseline mode starts cleanly at tail without interim lagged"
    );

    // After switching to s2, reading `at_boundary` preserves s1 tail (4, 5) before delivering s2 (seq 1).
    let s2_epoch = log.switch_session(Some("s2".into()));
    log.admit_json(
        "step/completed",
        serde_json::json!({"turn_id": "t1", "step_id": "s6", "status": "completed", "n": 6}),
    )
    .await
    .unwrap();
    let mut seen = Vec::new();
    while let Ok(Some(rec)) = at_boundary.cursor.try_recv() {
        if let CursorRecord::Event { event, .. } = rec {
            seen.push((event.session_id, event.seq));
        }
    }
    assert_eq!(seen, vec![(Some("s1".into()), 4), (Some("s1".into()), 5), (Some("s2".into()), 1)]);

    // Evict all remaining s1 entries in `s2`.
    for n in 7..=9 {
        log.admit_json(
            "step/completed",
            serde_json::json!({"turn_id": "t1", "step_id": format!("s{n}"), "status": "completed", "n": n}),
        )
        .await
        .unwrap();
    }
    assert!(log.lock().entries.iter().all(|e| e.session_id.as_deref() == Some("s2")));

    // Reconnecting with prior `s1` epoch resolves cleanly to `Baseline` in the current `s2` epoch.
    let mut stale_epoch_sub = log.subscribe_cursor(Some(&epoch), Some(1), false, Arc::new(|_| Some(true)), None);
    assert_eq!(stale_epoch_sub.mode, mare_protocol::SubscribeMode::Baseline);
    assert_eq!(stale_epoch_sub.session_epoch, s2_epoch);
    assert_eq!(stale_epoch_sub.seq, 5);
    assert!(
        stale_epoch_sub.cursor.try_recv().unwrap().is_none(),
        "baseline mode starts cleanly at tail without interim lagged"
    );

    let resumed_live = log.subscribe_cursor(Some(&s2_epoch), Some(4), false, Arc::new(|_| Some(true)), None);
    assert_eq!(resumed_live.mode, mare_protocol::SubscribeMode::Live);
    assert_eq!(resumed_live.session_epoch, s2_epoch);
    assert_eq!(resumed_live.seq, 5);
}

#[tokio::test]
async fn wire_projector_skips_when_inactive_and_redacts_tap_payload_when_active() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let log = EventLog::new();
    let has_entries = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_json = Arc::clone(&calls);
    let projector = mare_shared::wire_projection::WireProjector::new(
        Arc::clone(&has_entries),
        Arc::new(move |val| {
            calls_json.fetch_add(1, Ordering::SeqCst);
            if let Some(obj) = val.as_object_mut()
                && let Some(Value::String(s)) = obj.get_mut("text")
            {
                *s = s.replace("RAW_SECRET_99", "[[mare-value:redacted]]");
            }
        }),
        Arc::new(|s| s.to_owned()),
        Arc::new(|b| b.to_vec()),
        Arc::new(|| Box::new(NoopByteStream)),
    );
    log.set_wire_projector(projector);
    let mut cursor = log.open_cursor(Arc::new(|_| Some(true)));

    // Inactive: zero projector invocations.
    log.admit_json(
        "item/started",
        serde_json::json!({ "item_id": "i1", "item": { "type": "user_message" }, "text": "plain" }),
    )
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0, "inactive registry must not invoke projector");
    let _ = cursor.try_recv().unwrap().unwrap();

    // Active: both `wire_json` and ACP-consumed `TapEvent.payload` carry the projected value.
    has_entries.store(true, Ordering::Release);
    log.admit_json(
        "item/started",
        serde_json::json!({ "item_id": "i2", "item": { "type": "user_message" }, "text": "leak RAW_SECRET_99 here" }),
    )
    .await
    .unwrap();
    assert!(calls.load(Ordering::SeqCst) >= 1);
    let Some(CursorRecord::Event { event, .. }) = cursor.try_recv().unwrap() else {
        panic!("expected event");
    };
    assert!(!event.wire_json.contains("RAW_SECRET_99"));
    assert!(!event.payload.to_string().contains("RAW_SECRET_99"));
    assert_eq!(event.payload["text"], "leak [[mare-value:redacted]] here");
}

#[tokio::test]
async fn kernel_binds_projector_slot_eagerly_for_pure_rust_admit_and_pending_retained_flush() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // Kernel::new before wire_projection::install must still project pure-Rust admit_json calls.
    let kernel = Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let json_calls = Arc::clone(&calls);
    mare_shared::wire_projection::install(
        kernel.lua(),
        mare_shared::wire_projection::WireProjector::new(
            Arc::clone(&flag),
            Arc::new(move |val| {
                json_calls.fetch_add(1, Ordering::SeqCst);
                if let Some(obj) = val.as_object_mut()
                    && let Some(Value::String(s)) = obj.get_mut("message")
                {
                    *s = s.replace("PURE_RUST_SEC", "[[mare-value:rust]]");
                }
            }),
            Arc::new(|s| s.to_owned()),
            Arc::new(|b| b.to_vec()),
            Arc::new(|| Box::new(NoopByteStream)),
        ),
    );

    let log = kernel.event_log();
    log.set_limits(EventLogLimits { capacity: 1, ..EventLogLimits::default() });
    let (_tap, mut hcursor) = log.open_headless_cursor(Arc::new(|name| {
        Some(HeadlessSelection { selected: true, required: name == "turn/started" })
    }));

    // Pin capacity=1 with a required entry so `status/changed` is deferred into `pending_retained`,
    // then register the secret before draining and committing the retained entry.
    log.admit_json("turn/started", serde_json::json!({ "turn_id": "t1" })).await.unwrap();
    log.admit_json("status/changed", serde_json::json!({ "message": "deferred PURE_RUST_SEC" })).await.unwrap();
    flag.store(true, Ordering::Release);
    let first = hcursor.recv().await.unwrap();
    assert!(matches!(first.record, HeadlessRecord::Event(_)));

    // Direct pure-Rust `admit_json` without any prior Lua `tap_event` projects both entries.
    log.admit_json("status/changed", serde_json::json!({ "message": "direct PURE_RUST_SEC" })).await.unwrap();
    let flushed = hcursor.recv().await.unwrap();
    let HeadlessRecord::Event(ev1) = flushed.record else { panic!("expected retained event") };
    assert!(!ev1.wire_json.contains("PURE_RUST_SEC"), "retained wire leaked: {}", ev1.wire_json);
    assert!(ev1.wire_json.contains("[[mare-value:rust]]"), "retained wire not projected: {}", ev1.wire_json);

    let direct = hcursor.recv().await.unwrap();
    let HeadlessRecord::Event(ev2) = direct.record else { panic!("expected direct event") };
    assert!(!ev2.wire_json.contains("PURE_RUST_SEC"), "direct wire leaked: {}", ev2.wire_json);
    assert!(ev2.wire_json.contains("[[mare-value:rust]]"), "direct wire not projected: {}", ev2.wire_json);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "each committed event must be projected exactly once");
}

#[tokio::test]
async fn retained_baseline_reprojects_values_registered_while_event_is_waiting() {
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    let log = EventLog::with_limits(EventLogLimits { capacity: 1, ..EventLogLimits::default() });
    let active = Arc::new(AtomicBool::new(false));
    let revision = Arc::new(AtomicU64::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let for_projection = revision.clone();
    let for_revision = revision.clone();
    let projection_calls = calls.clone();
    log.set_wire_projector(
        mare_shared::wire_projection::WireProjector::new(
            active.clone(),
            Arc::new(move |value| {
                projection_calls.fetch_add(1, Ordering::SeqCst);
                if let Some(Value::String(text)) = value.get_mut("message") {
                    if for_projection.load(Ordering::SeqCst) >= 1 {
                        *text = text.replace("FIRST_SECRET", "[first]");
                    }
                    if for_projection.load(Ordering::SeqCst) >= 2 {
                        *text = text.replace("LATER_SECRET", "[later]");
                    }
                }
            }),
            Arc::new(str::to_owned),
            Arc::new(<[u8]>::to_vec),
            Arc::new(|| Box::new(NoopByteStream)),
        )
        .with_revision(Arc::new(move || for_revision.load(Ordering::SeqCst))),
    );
    let (_tap, mut fixed) =
        log.open_headless_cursor(Arc::new(|_| Some(HeadlessSelection { selected: true, required: false })));
    log.admit_json("agent/start", serde_json::json!({ "turn_id": "t" })).await.unwrap();
    log.admit_json("status/changed", serde_json::json!({ "message": "FIRST_SECRET LATER_SECRET" })).await.unwrap();
    revision.store(1, Ordering::SeqCst);
    active.store(true, Ordering::SeqCst);

    let (subscription, _, retained) =
        log.subscribe_cursor_with_retained(None, None, false, Arc::new(|_| Some(true)), None, &|name| {
            name == "status/changed"
        });
    assert_eq!(retained["status/changed"]["message"], "[first] LATER_SECRET");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    revision.store(2, Ordering::SeqCst);
    let anchor = log.reanchor_cursor_at_tail(subscription.cursor.id(), &|name| name == "status/changed").unwrap();
    assert_eq!(anchor.retained["status/changed"]["message"], "[first] [later]");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let anchor = log.reanchor_cursor_at_tail(subscription.cursor.id(), &|name| name == "status/changed").unwrap();
    assert_eq!(anchor.retained["status/changed"]["message"], "[first] [later]");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "same revision must reuse the retained projection");

    let _ = fixed.recv().await.unwrap();
    let HeadlessRecord::Event(event) = fixed.recv().await.unwrap().record else { panic!("expected retained event") };
    assert_eq!(event.payload["message"], "[first] [later]");
}

#[tokio::test]
async fn retained_event_projection_expansion_waits_without_reprojecting() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let log = EventLog::new();
    log.set_limits(EventLogLimits { capacity: 2, max_bytes: 600, max_event_bytes: 600, ..EventLogLimits::default() });
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    log.set_wire_projector(mare_shared::wire_projection::WireProjector::new(
        Arc::new(AtomicBool::new(true)),
        Arc::new(move |value| {
            counted.fetch_add(1, Ordering::SeqCst);
            if let Some(Value::String(text)) = value.get_mut("message") {
                *text = text.replace("SECRET", &"x".repeat(450));
            }
        }),
        Arc::new(str::to_owned),
        Arc::new(<[u8]>::to_vec),
        Arc::new(|| Box::new(NoopByteStream)),
    ));
    let (_tap, mut cursor) = log.open_headless_cursor(Arc::new(|name| {
        Some(HeadlessSelection { selected: true, required: name == "agent/start" })
    }));
    log.admit_json("agent/start", serde_json::json!({ "turn_id": "t" })).await.unwrap();
    log.admit_json("status/changed", serde_json::json!({ "message": "SECRET" })).await.unwrap();
    let _ = cursor.recv().await.unwrap();
    let retained = cursor.recv().await.unwrap();
    let HeadlessRecord::Event(event) = retained.record else { panic!("expected retained event") };
    assert!(!event.wire_json.contains("SECRET"), "{}", event.wire_json);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "each event is projected once even when expansion delays it");
}

#[tokio::test]
async fn retained_event_reprojects_if_registry_changes_before_commit() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let log = EventLog::new();
    log.set_limits(EventLogLimits { capacity: 2, max_bytes: 600, max_event_bytes: 600, ..EventLogLimits::default() });
    let revision = Arc::new(AtomicU64::new(1));
    let for_projection = revision.clone();
    let for_revision = revision.clone();
    log.set_wire_projector(
        mare_shared::wire_projection::WireProjector::new(
            Arc::new(AtomicBool::new(true)),
            Arc::new(move |value| {
                if let Some(Value::String(text)) = value.get_mut("message") {
                    *text = text.replace("FIRST", &"x".repeat(440));
                    if for_projection.load(Ordering::SeqCst) > 1 {
                        *text = text.replace("LATER", "[redacted]");
                    }
                }
            }),
            Arc::new(str::to_owned),
            Arc::new(<[u8]>::to_vec),
            Arc::new(|| Box::new(NoopByteStream)),
        )
        .with_revision(Arc::new(move || for_revision.load(Ordering::SeqCst))),
    );
    let (_tap, mut cursor) = log.open_headless_cursor(Arc::new(|name| {
        Some(HeadlessSelection { selected: true, required: name == "agent/start" })
    }));
    log.admit_json("agent/start", serde_json::json!({ "turn_id": "t" })).await.unwrap();
    log.admit_json("status/changed", serde_json::json!({ "message": "FIRST LATER" })).await.unwrap();
    revision.store(2, Ordering::SeqCst);
    let _ = cursor.recv().await.unwrap();
    let retained = cursor.recv().await.unwrap();
    let HeadlessRecord::Event(event) = retained.record else { panic!("expected retained event") };
    assert!(!event.wire_json.contains("LATER"), "late registered value leaked: {}", event.wire_json);
    assert!(event.wire_json.contains("[redacted]"), "{}", event.wire_json);
}

#[tokio::test]
async fn projected_event_may_fit_when_raw_secret_exceeds_wire_limit() {
    use std::sync::atomic::AtomicBool;

    let log = EventLog::new();
    log.set_limits(EventLogLimits { capacity: 2, max_bytes: 1024, max_event_bytes: 250, ..EventLogLimits::default() });
    let secret = "s".repeat(300);
    let raw = secret.clone();
    log.set_wire_projector(mare_shared::wire_projection::WireProjector::new(
        Arc::new(AtomicBool::new(true)),
        Arc::new(move |value| {
            if let Some(Value::String(text)) = value.get_mut("text") {
                *text = text.replace(&raw, "[redacted]");
            }
        }),
        Arc::new(str::to_owned),
        Arc::new(<[u8]>::to_vec),
        Arc::new(|| Box::new(NoopByteStream)),
    ));
    let mut cursor = log.open_cursor(Arc::new(|_| Some(true)));
    log.admit_json("agent/tool-start", serde_json::json!({ "text": secret })).await.unwrap();
    let Some(CursorRecord::Event { event, .. }) = cursor.try_recv().unwrap() else {
        panic!("an event under the wire limit after projection must be admitted");
    };
    assert_eq!(event.payload["text"], "[redacted]");
}

#[tokio::test]
async fn turn_scoped_event_requires_non_empty_turn_id_and_preserves_contiguous_seq() {
    let log = EventLog::new();
    let mut cursor = log.open_cursor(Arc::new(|_| Some(true)));

    let missing = log
        .admit_json_scoped("probe/turn-ev", serde_json::json!({"ok_flag": true}), Some(mare_protocol::EventScope::Turn))
        .await;
    assert!(missing.is_err(), "turn-scoped event without turn_id must fail");
    assert!(matches!(cursor.try_recv().unwrap(), Some(CursorRecord::Dropped { count: 1, .. })));

    let empty = log
        .admit_json_scoped(
            "probe/turn-ev",
            serde_json::json!({"turn_id": "   "}),
            Some(mare_protocol::EventScope::Turn),
        )
        .await;
    assert!(empty.is_err(), "turn-scoped event with blank turn_id must fail");
    assert!(matches!(cursor.try_recv().unwrap(), Some(CursorRecord::Dropped { count: 1, .. })));

    // Valid turn-scoped event must still start cleanly at seq = 1 (rejected calls did not burn seq).
    log.admit_json_scoped(
        "probe/turn-ev",
        serde_json::json!({"turn_id": "t-42", "step": 1}),
        Some(mare_protocol::EventScope::Turn),
    )
    .await
    .unwrap();
    let rec = cursor.try_recv().unwrap().unwrap();
    let CursorRecord::Event { event, selected } = rec else { panic!("expected event, got {rec:?}") };
    assert!(selected);
    assert_eq!(event.seq, 1);
    assert_eq!(event.turn_id.as_deref(), Some("t-42"));
}

#[tokio::test]
async fn session_scoped_event_does_not_inherit_active_turn_id() {
    let log = EventLog::new();
    let mut cursor = log.open_cursor(Arc::new(|_| Some(true)));

    log.admit_json("turn/started", serde_json::json!({"turn_id": "t-9", "trigger": "prompt"})).await.unwrap();
    log.admit_json("host/notice", serde_json::json!({"level": "info", "text": "hi"})).await.unwrap();
    log.admit_json("turn/completed", serde_json::json!({"turn_id": "t-9", "status": "completed", "usage": {}}))
        .await
        .unwrap();

    let _ = cursor.try_recv().unwrap().unwrap();
    let CursorRecord::Event { event: notice_ev, .. } = cursor.try_recv().unwrap().unwrap() else {
        panic!("expected notice event");
    };
    assert_eq!(notice_ev.name, "host/notice");
    assert_eq!(notice_ev.turn_id, None, "session event without payload.turn_id must not inherit active_turn_id");
}

#[tokio::test]
async fn dual_runtime_isolated_taps_and_keyed_retained_state() {
    use mlua::LuaSerdeExt;

    let contract_src = r#"return { widget = { methods = {}, events = {
        ['activity-started'] = {
            doc = 'Activity started', expose = 'both', delivery = 'lossy', ignorable = false,
            retain = { key = 'item_id', cleared_by = 'widget/activity-completed' },
            data = { type = 'object', properties = { item_id = { type = 'string' }, tag = { type = 'string' } }, required = { 'item_id' } },
            example = { item_id = 'a', tag = 'A' },
        },
        ['activity-completed'] = {
            doc = 'Activity completed', expose = 'lua', delivery = 'lossy', ignorable = true,
            data = { type = 'object', properties = { item_id = { type = 'string' } }, required = { 'item_id' } },
            example = { item_id = 'a' },
        },
    } } }"#;

    let k_a = Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
    let k_b = Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
    let ca: mlua::Table = k_a.lua().load(contract_src).eval().unwrap();
    let cb: mlua::Table = k_b.lua().load(contract_src).eval().unwrap();
    k_a.install_service_contracts(ca).unwrap();
    k_b.install_service_contracts(cb).unwrap();

    k_a.0.event_log.set_limits(EventLogLimits { capacity: 1, ..EventLogLimits::default() });
    k_b.0.event_log.set_limits(EventLogLimits { capacity: 4, ..EventLogLimits::default() });
    let epoch_a = k_a.0.event_log.switch_session(Some("rt-a".into()));
    let epoch_b = k_b.0.event_log.switch_session(Some("rt-b".into()));

    let (pin_a, mut hcur_a) =
        k_a.open_headless_cursor(Arc::new(|_| Some(HeadlessSelection { required: true, selected: true })));
    let mut cur_a = k_a.open_cursor(Arc::new(|_| Some(true)));
    let mut cur_b = k_b.open_cursor(Arc::new(|_| Some(true)));

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let ba = Arc::clone(&barrier);
    let bb = Arc::clone(&barrier);
    let log_a = k_a.event_log();
    let log_b = k_b.event_log();

    let emit_a = async {
        // Pin k_a capacity=1 so `widget/activity-started` goes into `pending_retained` while k_b completes `shared`.
        log_a.admit_json("turn/started", serde_json::json!({"turn_id": "ta", "trigger": "prompt"})).await.unwrap();
        ba.wait().await;
        log_a
            .admit_json("widget/activity-started", serde_json::json!({"item_id": "shared", "tag": "from-A"}))
            .await
            .unwrap();
        log_a
            .admit_json("widget/activity-started", serde_json::json!({"item_id": "only-a", "tag": "A2"}))
            .await
            .unwrap();
    };
    let emit_b = async {
        bb.wait().await;
        log_b.admit_json("turn/started", serde_json::json!({"turn_id": "tb", "trigger": "prompt"})).await.unwrap();
        log_b
            .admit_json("widget/activity-started", serde_json::json!({"item_id": "shared", "tag": "from-B"}))
            .await
            .unwrap();
        let comp =
            mlua::MultiValue::from_vec(vec![k_b.lua().to_value(&serde_json::json!({"item_id": "shared"})).unwrap()]);
        k_b.tap_event("widget/activity-completed", &comp).await;
    };
    tokio::join!(emit_a, emit_b);

    // k_a must still retain `"shared"` (`tag == "from-A"`) while k_b cleared `"shared"` cleanly.
    let (_, snap_a) = k_a
        .subscribe_with_baseline(
            1,
            None,
            None,
            false,
            Arc::new(|_| Some(true)),
            None,
            &|_| true,
            Default::default(),
            true,
            None,
        )
        .unwrap();
    let (_, snap_b) = k_b
        .subscribe_with_baseline(
            1,
            None,
            None,
            false,
            Arc::new(|_| Some(true)),
            None,
            &|_| true,
            Default::default(),
            true,
            None,
        )
        .unwrap();
    let snap_a = snap_a.unwrap();
    let snap_b = snap_b.unwrap();
    let retained_a = snap_a.retained["events"]["widget/activity-started"].as_array().unwrap();
    assert_eq!(retained_a.len(), 2);
    assert!(retained_a.iter().any(|v| v["item_id"] == "shared" && v["tag"] == "from-A"));
    assert!(snap_b.retained["events"].get("widget/activity-started").is_none());

    let _ = hcur_a.try_recv();
    drop((pin_a, hcur_a));
    let mut seqs_a = Vec::new();
    let mut lagged_a = None;
    while let Ok(Some(rec)) = cur_a.try_recv() {
        match rec {
            CursorRecord::Lagged { from_seq, to_seq } => lagged_a = Some((from_seq, to_seq)),
            CursorRecord::Event { event, .. } => {
                assert_eq!(event.session_id.as_deref(), Some("rt-a"));
                assert_eq!(event.session_epoch, epoch_a);
                seqs_a.push(event.seq);
            }
            CursorRecord::Dropped { .. } => panic!("unexpected drop on k_a"),
        }
    }
    let mut seqs_b = Vec::new();
    while let Ok(Some(rec)) = cur_b.try_recv() {
        let CursorRecord::Event { event, .. } = rec else { panic!("unexpected non-event on k_b: {rec:?}") };
        assert_eq!(event.session_id.as_deref(), Some("rt-b"));
        assert_eq!(event.session_epoch, epoch_b);
        seqs_b.push(event.seq);
    }
    assert_eq!(lagged_a, Some((1, 2)));
    assert_eq!(seqs_a, vec![3]);
    assert_eq!(seqs_b, vec![1, 2, 3]);
}

#[tokio::test]
async fn same_host_session_switch_tail_baseline_stale_and_epoch_eviction_boundary() {
    let k = Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
    let log = k.event_log();
    log.set_limits(EventLogLimits { capacity: 2, ..EventLogLimits::default() });
    let e1 = log.switch_session(Some("s1".into()));

    // Cut a baseline handle in e1.
    let (_, snap1) = k
        .subscribe_with_baseline(
            7,
            None,
            None,
            false,
            Arc::new(|_| Some(true)),
            None,
            &|_| true,
            Default::default(),
            true,
            Some(Arc::new(|_| true)),
        )
        .unwrap();
    let b1 = snap1.unwrap().baseline_id.expect("page_access enabled creates baseline handle");
    assert!(k.owns_baseline(7, &b1));

    let mut cur_e1_to_e2 = log.open_cursor(Arc::new(|_| Some(true)));
    let mut cur_slow = log.open_cursor(Arc::new(|_| Some(true)));
    log.admit_json("host/notice", serde_json::json!({"level":"info","text":"s1-1"})).await.unwrap();
    log.admit_json("host/notice", serde_json::json!({"level":"info","text":"s1-2"})).await.unwrap();
    // Consume only s1-1 on cur_e1_to_e2, leaving s1-2 unread in the ring.
    let Some(CursorRecord::Event { event: got1, .. }) = cur_e1_to_e2.try_recv().unwrap() else { panic!() };
    assert_eq!((got1.seq, got1.payload["text"].as_str()), (1, Some("s1-1")));

    // Switch session to s2; e2 starts at seq 1, and reading e1's baseline handle must return baseline.stale.
    let e2 = log.switch_session(Some("s2".into()));
    assert_ne!(e1, e2);
    let stale_err = k
        .read_baseline_page(7, &b1, "part", None, None)
        .expect_err("reading baseline from previous epoch must return baseline.stale");
    assert_eq!(stale_err.code(), "baseline.stale");
    assert!(!k.owns_baseline(7, &b1));

    // Verify e1 -> e2 boundary when ring head is e2 seq 1 (no bogus Lagged emitted across epochs).
    log.admit_json("host/notice", serde_json::json!({"level":"info","text":"s2-1"})).await.unwrap();
    // Note: capacity=2, so s2-1 evicted s1-1 while keeping s1-2 (tail of s1!) and s2-1.
    let mut seq_pairs = Vec::new();
    while let Ok(Some(CursorRecord::Event { event, .. })) = cur_e1_to_e2.try_recv() {
        seq_pairs.push((event.session_epoch, event.seq, event.payload["text"].as_str().unwrap().to_owned()));
    }
    assert_eq!(seq_pairs, vec![(e1.clone(), 2, "s1-2".to_owned()), (e2.clone(), 1, "s2-1".to_owned())]);

    // Emit s2-2 (ring now holds [s2-1, s2-2]; cur_slow missed s1-1..2, head is e2 seq 1 -> clean jump to s2-1).
    log.admit_json("host/notice", serde_json::json!({"level":"info","text":"s2-2"})).await.unwrap();
    let Some(CursorRecord::Event { event: slow_first, .. }) = cur_slow.try_recv().unwrap() else { panic!() };
    assert_eq!((slow_first.session_epoch.as_str(), slow_first.seq), (e2.as_str(), 1));

    // Verify e1 pending_retained does not leak across switch_session and e2 retained stays isolated.
    let k_ret = Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
    let log_ret = k_ret.event_log();
    log_ret.set_limits(EventLogLimits { capacity: 1, ..EventLogLimits::default() });
    let _e1_ret = log_ret.switch_session(Some("s1".into()));
    let (pin_ret, mut hcur_ret) =
        k_ret.open_headless_cursor(Arc::new(|_| Some(HeadlessSelection { required: true, selected: true })));
    log_ret.admit_json("turn/started", serde_json::json!({"turn_id": "t1", "trigger": "prompt"})).await.unwrap();
    log_ret
        .admit_json("status/changed", serde_json::json!({"state": "working", "message": "e1-pending"}))
        .await
        .unwrap();
    let _e2_ret = log_ret.switch_session(Some("s2".into()));
    let _ = hcur_ret.try_recv();
    assert!(hcur_ret.try_recv().is_none());
    assert_eq!(log_ret.session_tail().1, 0);
    drop((pin_ret, hcur_ret));
    let (_, snap_after_switch) = k_ret
        .subscribe_with_baseline(
            1,
            None,
            None,
            false,
            Arc::new(|_| Some(true)),
            None,
            &|_| true,
            Default::default(),
            true,
            None,
        )
        .unwrap();
    let snap_after_switch = snap_after_switch.unwrap();
    assert!(
        snap_after_switch.retained["events"].get("status/changed").is_none(),
        "stale e1 pending_retained must be cleared on epoch rotation: {:?}",
        snap_after_switch.retained
    );
    log_ret.admit_json("status/changed", serde_json::json!({"state": "idle", "message": "e2-fresh"})).await.unwrap();
    let (_, snap_e2) = k_ret
        .subscribe_with_baseline(
            2,
            None,
            None,
            false,
            Arc::new(|_| Some(true)),
            None,
            &|_| true,
            Default::default(),
            true,
            None,
        )
        .unwrap();
    assert_eq!(snap_e2.unwrap().retained["events"]["status/changed"]["message"], "e2-fresh");

    // Emit s2-3, s2-4, s2-5 so cur_slow falls behind e2 seq 2..3 (head_seq = 4 > 1 -> Lagged { 1..=3 } or { 2..=3 }).
    for n in 3..=5 {
        log.admit_json("host/notice", serde_json::json!({"level":"info","text":format!("s2-{n}")})).await.unwrap();
    }
    let Some(CursorRecord::Lagged { from_seq, to_seq }) = cur_slow.try_recv().unwrap() else {
        panic!("expected e2 Lagged")
    };
    assert_eq!((from_seq, to_seq), (2, 3));
}

#[tokio::test]
async fn cursors_lock_free_decode_preserves_lagged_dropped_and_events() {
    let log = EventLog::new();
    log.set_limits(EventLogLimits { capacity: 2, ..EventLogLimits::default() });
    let epoch = log.switch_session(Some("lockfree".into()));
    let mut cur_fast = log.open_cursor(Arc::new(|_| Some(true)));
    let mut cur_lag = log.open_cursor(Arc::new(|name| Some(name == "host/notice")));

    // 1. Scoped drop is recorded and surfaced cleanly without touching decode path.
    assert!(
        log.admit_json_scoped("probe/drop", serde_json::json!({ "n": 0 }), Some(mare_protocol::EventScope::Turn))
            .await
            .is_err()
    );
    assert!(matches!(
        cur_fast.try_recv().unwrap(),
        Some(CursorRecord::Dropped { count: 1, session_epoch, .. }) if session_epoch == epoch
    ));

    // 2. Admit 3 events into capacity-2 ring: seq 1 is evicted before cur_lag reads.
    for i in 1..=3 {
        log.admit_json("host/notice", serde_json::json!({ "level": "info", "text": format!("msg-{i}") }))
            .await
            .unwrap();
    }
    let Some(CursorRecord::Lagged { from_seq, to_seq }) = cur_lag.try_recv().unwrap() else {
        panic!("expected Lagged before reading ring");
    };
    assert_eq!((from_seq, to_seq), (1, 1));

    // 3. Remaining events decode outside the EventLog lock with exact seq, epoch, and payload.
    let Some(CursorRecord::Event { event: ev2, selected: sel2 }) = cur_lag.try_recv().unwrap() else {
        panic!("expected seq 2 event");
    };
    assert!(sel2);
    assert_eq!((ev2.seq, ev2.session_epoch.as_str()), (2, epoch.as_str()));
    assert_eq!(ev2.payload["text"], "msg-2");

    let Some(CursorRecord::Event { event: ev3, .. }) = cur_lag.try_recv().unwrap() else {
        panic!("expected seq 3 event");
    };
    assert_eq!((ev3.seq, ev3.payload["text"].as_str()), (3, Some("msg-3")));
    assert!(cur_lag.try_recv().unwrap().is_none());
}

#[test]
fn concurrent_retain_last_preserves_admission_order_when_older_projection_finishes_later() {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    let log = EventLog::new();
    let (older_entered_tx, older_entered_rx) = mpsc::channel::<()>();
    let (release_older_tx, release_older_rx) = mpsc::channel::<()>();
    let release_older_rx = Arc::new(std::sync::Mutex::new(release_older_rx));
    let older_entered_tx = Arc::new(std::sync::Mutex::new(Some(older_entered_tx)));

    log.set_wire_projector(mare_shared::wire_projection::WireProjector::new(
        Arc::new(AtomicBool::new(true)),
        Arc::new(move |value| {
            if value.get("message").and_then(Value::as_str) == Some("older") {
                if let Some(tx) = older_entered_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let _ = release_older_rx.lock().unwrap().recv();
            }
        }),
        Arc::new(str::to_owned),
        Arc::new(<[u8]>::to_vec),
        Arc::new(|| Box::new(NoopByteStream)),
    ));

    let mut cursor = log.open_cursor(Arc::new(|_| Some(true)));
    let log_t1 = log.clone();
    let t1 = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(log_t1.admit_json(
            "status/changed",
            serde_json::json!({ "state": "working", "message": "older", "session": {}, "seq": 1 }),
        ))
        .unwrap();
    });

    // Wait until t1 has claimed the earlier admission generation and entered lock-free projection.
    older_entered_rx.recv().unwrap();

    // Emit the newer retain=last event while t1 is still projecting outside the EventLog lock.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(log.admit_json(
        "status/changed",
        serde_json::json!({ "state": "idle", "message": "newer", "session": {}, "seq": 2 }),
    ))
    .unwrap();

    // Release t1 so its older projection finishes after t2 already recorded "newer".
    release_older_tx.send(()).unwrap();
    t1.join().unwrap();

    let mut st = log.lock();
    let retained = super::log::collect_retained_locked(&mut st, &|_| true);
    drop(st);
    assert_eq!(
        retained["status/changed"]["message"], "newer",
        "older in-flight projection must not overwrite newer retain=last snapshot: {:?}",
        retained
    );

    let mut streamed = Vec::new();
    while let Some(rec) = cursor.try_recv().unwrap() {
        if let CursorRecord::Event { event, .. } = rec {
            streamed.push(event.payload["message"].as_str().unwrap_or_default().to_string());
        }
    }
    assert_eq!(
        streamed,
        vec!["newer"],
        "older superseded retain=last event must not emit onto live stream after newer event"
    );
}

#[test]
fn stale_clear_retained_does_not_delete_newer_retained_or_pending_entry() {
    let log = EventLog::new();
    let mut st = log.lock();
    let epoch = st.session_epoch.clone();
    let mut projected = None;
    let payload = serde_json::json!({ "item_id": "k1", "tag": "newer" });
    super::log::project_payload(&st, &payload, &mut projected);
    let cached = projected.as_ref().expect("projection cache initialized");

    super::log::record_retained_locked(&mut st, "widget/activity-started", &Some("k1".into()), cached, 2);
    st.pending_retained.push(super::log::PendingRetained {
        name: "widget/activity-started".into(),
        payload,
        session_epoch: epoch,
        key: Some("k1".into()),
        ignorable: false,
        encoded_bytes: 32,
        projected,
        admit_gen: 2,
    });

    // A stale clear with an earlier admission generation must be ignored completely.
    super::log::clear_retained_locked(&mut st, "widget/activity-started", "k1", 1);

    let retained = super::log::collect_retained_locked(&mut st, &|_| true);
    assert_eq!(
        retained["widget/activity-started"],
        serde_json::json!([{ "item_id": "k1", "tag": "newer" }]),
        "stale clear (gen=1) must not remove newer retained value (gen=2)"
    );
    assert_eq!(st.pending_retained.len(), 1, "stale clear (gen=1) must not drop newer pending_retained entry (gen=2)");

    // A newer clear (gen=3) removes both retained and pending entries.
    super::log::clear_retained_locked(&mut st, "widget/activity-started", "k1", 3);
    assert!(super::log::collect_retained_locked(&mut st, &|_| true).is_empty());
    assert!(st.pending_retained.is_empty());
}

#[tokio::test]
async fn deferred_event_reprojects_new_secret_across_truncation_boundary() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let kernel = Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
    let log = &kernel.0.event_log;
    let max_event_bytes = 384usize;
    log.set_limits(EventLogLimits {
        capacity: 1,
        max_event_bytes,
        block_limit: Duration::from_secs(5),
        ..EventLogLimits::default()
    });
    log.set_session(Some("sess".into()));

    let active = Arc::new(AtomicBool::new(false));
    let revision = Arc::new(AtomicU64::new(0));
    let secret = "SECRET_BOUNDARY_TOKEN_987654321";
    let secret_owned = secret.to_string();
    let active_for_fn = Arc::clone(&active);
    let rev_for_fn = Arc::clone(&revision);
    let projector = mare_shared::wire_projection::WireProjector::new(
        Arc::clone(&active),
        Arc::new(move |val| {
            if !active_for_fn.load(Ordering::Acquire) {
                return;
            }
            fn walk(v: &mut Value, needle: &str) {
                match v {
                    Value::String(s) => {
                        if s.contains(needle) {
                            *s = s.replace(needle, "[[mare-value:redacted]]");
                        }
                    }
                    Value::Array(items) => {
                        for item in items {
                            walk(item, needle);
                        }
                    }
                    Value::Object(map) => {
                        for val in map.values_mut() {
                            walk(val, needle);
                        }
                    }
                    _ => {}
                }
            }
            walk(val, &secret_owned);
        }),
        Arc::new(str::to_owned),
        Arc::new(<[u8]>::to_vec),
        Arc::new(|| Box::new(NoopByteStream)),
    )
    .with_revision(Arc::new(move || rev_for_fn.load(Ordering::Acquire)));
    log.set_wire_projector(projector);

    // Pin ring capacity=1 with a fixed cursor so the next Required event defers commit.
    let (_handle, mut cursor) =
        kernel.open_headless_cursor(Arc::new(|_| Some(HeadlessSelection { required: true, selected: true })));
    log.admit_json("turn/started", serde_json::json!({ "turn_id": "t0", "trigger": "prompt" })).await.unwrap();

    // Find a prefix length that causes `encode_bounded_wire_event` to slice right through `secret`,
    // leaving `"SECRET_BOUNDARY_"` intact while cutting off `"TOKEN_987654321"`.
    let make_text = |prefix_len: usize| format!("{}{}{}", "a".repeat(prefix_len), secret, "z".repeat(400));
    let identity = {
        let st = log.lock();
        super::log::EventIdentity::current(&st)
    };
    let split_prefix = (20..300)
        .find(|&n| {
            let mut probe = serde_json::json!({
                "turn_id": "t1",
                "status": "completed",
                "text": make_text(n),
            });
            super::bounds::encode_bounded_wire_event(&identity, "turn/completed", 2, false, &mut probe, max_event_bytes)
                .ok()
                .flatten()
                .is_some_and(|wire| wire.contains("SECRET_BOUNDARY_") && !wire.contains(secret))
        })
        .expect("expected a prefix length where truncation splits the secret");

    let req_payload = serde_json::json!({
        "turn_id": "t1",
        "status": "completed",
        "text": make_text(split_prefix),
    });
    let mut waiting = Box::pin(log.admit_json("turn/completed", req_payload));
    assert!(poll_once(waiting.as_mut()).is_pending(), "required event must wait while pinned ring is full");

    // Also admit a retained lossy event (`status/changed`) with the secret straddling the boundary.
    let retained_prefix = (20..300)
        .find(|&n| {
            let mut probe = serde_json::json!({
                "state": "working",
                "message": make_text(n),
                "session": {},
            });
            super::bounds::encode_bounded_wire_event(&identity, "status/changed", 3, false, &mut probe, max_event_bytes)
                .ok()
                .flatten()
                .is_some_and(|wire| wire.contains("SECRET_BOUNDARY_") && !wire.contains(secret))
        })
        .expect("expected a prefix length where retained truncation splits the secret");
    log.admit_json(
        "status/changed",
        serde_json::json!({
            "state": "working",
            "message": make_text(retained_prefix),
            "session": {},
        }),
    )
    .await
    .unwrap();

    // Register the secret AFTER initial admission/truncation but BEFORE deferred commit & retained snapshot.
    active.store(true, Ordering::Release);
    revision.fetch_add(1, Ordering::AcqRel);

    // Unpin the ring so the waiting Required event commits now under the new projector revision.
    let _ = cursor.try_recv().expect("consume pinning event");
    waiting.await.unwrap();
    let HeadlessRecord::Event(committed) = cursor.try_recv().expect("committed required event").record else {
        panic!("expected committed event");
    };
    assert!(
        !committed.wire_json.contains("SECRET_BOUNDARY"),
        "deferred required event leaked truncated secret prefix: {}",
        committed.wire_json
    );
    assert!(
        committed.wire_json.len() <= max_event_bytes,
        "re-projected deferred event must remain within max_event_bytes ({} > {max_event_bytes})",
        committed.wire_json.len()
    );

    // Inspect retained snapshot taken after the new secret was registered.
    let mut st = log.lock();
    let retained = super::log::collect_retained_locked(&mut st, &|_| true);
    drop(st);
    let retained_json = retained["status/changed"].to_string();
    assert!(
        !retained_json.contains("SECRET_BOUNDARY"),
        "retained snapshot leaked truncated secret prefix: {retained_json}"
    );
}
