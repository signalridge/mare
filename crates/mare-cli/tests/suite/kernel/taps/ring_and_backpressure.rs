use super::super::{kernel, src};

#[tokio::test]
async fn a_full_event_log_drops_unretained_lossy_and_waits_for_required_ones() {
    tokio::task::LocalSet::new().run_until(tap_inner()).await
}

async fn tap_inner() {
    let k = kernel();
    k.set_event_log_limits(mare_kernel::EventLogLimits { capacity: 2, ..mare_kernel::EventLogLimits::default() });
    let (_handle, mut hcursor) = k.open_headless_cursor(std::sync::Arc::new(|_| {
        Some(mare_kernel::HeadlessSelection { required: true, selected: true })
    }));
    // Fill capacity 2 (`host/notice` is Lossy, ignorable, unretained), then overflow by 3.
    for _ in 0..5 {
        k.emit("host/notice", mlua::MultiValue::new()).await;
    }
    assert!(matches!(hcursor.try_recv().unwrap().record, mare_kernel::HeadlessRecord::Event(_)));
    assert!(matches!(hcursor.try_recv().unwrap().record, mare_kernel::HeadlessRecord::Event(_)));
    let marker = hcursor.try_recv().unwrap();
    assert!(
        matches!(marker.record, mare_kernel::HeadlessRecord::Dropped { count: 3, .. }),
        "expected Dropped {{ count: 3 }}, got {:?}",
        marker.record
    );

    // Fill capacity 2 again, then a Required event waits until the fixed cursor reads one.
    k.emit("host/notice", mlua::MultiValue::new()).await;
    k.emit("host/notice", mlua::MultiValue::new()).await;
    let k2 = k.clone();
    let done = tokio::task::spawn_local(async move { k2.emit("item/completed", mlua::MultiValue::new()).await });
    tokio::task::yield_now().await;
    assert!(!done.is_finished(), "the producer waits while the fixed cursor holds the log full");
    assert!(hcursor.try_recv().is_some());
    done.await.unwrap();
    assert!(matches!(hcursor.try_recv().unwrap().record, mare_kernel::HeadlessRecord::Event(_)));
    let end = hcursor.try_recv().unwrap();
    let mare_kernel::HeadlessRecord::Event(ev) = end.record else { panic!("expected event") };
    assert_eq!(ev.name, "item/completed");
}

#[tokio::test]
async fn retained_baselines_hide_values_registered_after_the_event_was_queued() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            k.set_event_log_limits(mare_kernel::EventLogLimits {
                capacity: 1,
                ..mare_kernel::EventLogLimits::default()
            });
            let (_tap, mut fixed) = k.open_headless_cursor(std::sync::Arc::new(|_| {
                Some(mare_kernel::HeadlessSelection { selected: true, required: false })
            }));
            k.event_log().admit_json("agent/start", serde_json::json!({ "turn_id": "t" })).await.unwrap();
            k.event_log()
                .admit_json(
                    "status/changed",
                    serde_json::json!({ "message": "FIRST_BASELINE_SECRET LATER_BASELINE_SECRET" }),
                )
                .await
                .unwrap();
            k.lua().load("_G.FIRST_LEASE = mare.projection.register('FIRST_BASELINE_SECRET')").exec().unwrap();

            let (sub, snapshot) = k
                .subscribe_with_baseline(
                    0,
                    None,
                    None,
                    false,
                    std::sync::Arc::new(|_| Some(true)),
                    None,
                    &|name| name == "status/changed",
                    Default::default(),
                    true,
                    Some(std::sync::Arc::new(|_| true)),
                )
                .unwrap();
            let snapshot = snapshot.expect("new subscription requires a baseline");
            let first = snapshot.retained["events"]["status/changed"]["message"].as_str().unwrap();
            assert!(!first.contains("FIRST_BASELINE_SECRET"), "registered value leaked into baseline: {first}");
            assert!(first.contains("[[mare-value:"));
            assert!(first.contains("LATER_BASELINE_SECRET"));

            k.lua().load("_G.LATER_LEASE = mare.projection.register('LATER_BASELINE_SECRET')").exec().unwrap();
            let snapshot = k
                .reanchor_with_baseline(
                    0,
                    sub.cursor.id(),
                    &|name| name == "status/changed",
                    Default::default(),
                    true,
                    Some(std::sync::Arc::new(|_| true)),
                )
                .unwrap()
                .expect("active cursor can be reanchored");
            let later = snapshot.retained["events"]["status/changed"]["message"].as_str().unwrap();
            assert!(!later.contains("FIRST_BASELINE_SECRET") && !later.contains("LATER_BASELINE_SECRET"), "{later}");
            assert_eq!(later.matches("[[mare-value:").count(), 2);
            let _ = fixed.recv().await.unwrap();
            let mare_kernel::HeadlessRecord::Event(event) = fixed.recv().await.unwrap().record else {
                panic!("expected the queued retained event");
            };
            let message = event.payload["message"].as_str().unwrap();
            assert!(
                !message.contains("FIRST_BASELINE_SECRET") && !message.contains("LATER_BASELINE_SECRET"),
                "{message}"
            );
        })
        .await
}

#[tokio::test]
async fn a_filtered_tap_gets_only_the_events_it_wants() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let mut only = k.open_cursor(std::sync::Arc::new(|e| (e == "status/changed").then_some(true)));
            let mut all = k.open_cursor(std::sync::Arc::new(|_| Some(true)));
            for ev in ["item/completed", "status/changed", "ui/notify"] {
                k.emit(ev, mlua::MultiValue::new()).await;
            }
            let names = |c: &mut mare_kernel::EventCursor| {
                std::iter::from_fn(|| c.try_recv().ok().flatten())
                    .filter_map(|rec| match rec {
                        mare_kernel::CursorRecord::Event { event, selected } if selected => Some(event.name),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(names(&mut only), ["status/changed"]);
            assert_eq!(names(&mut all), ["item/completed", "status/changed", "ui/notify"]);
        })
        .await
}

/// Private provider hooks can contain credentials and must never reach any frontend tap.
#[tokio::test]
async fn provider_hooks_remain_internal_even_for_unfiltered_taps() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let def = k
                .load_def(&src(
                    "provider-hooks",
                    r#"return {
            name = "provider-hooks",
            contracts = {
                provider = {
                    owner = "provider-hooks",
                    events = {
                        request = { doc = "wire request hook", mode = "waterfall", expose = "process" },
                        transport = { doc = "wire transport hook", mode = "bail", expose = "process" },
                        event = { doc = "wire event hook", mode = "emit", expose = "process" },
                    },
                },
            },
            apply = function(ctx)
                ctx:on("provider/request", function(w, next) w.rewritten = true; return next(w) end)
                ctx:on("provider/transport", function() return "handled" end)
                ctx:on("provider/event", function() mare._provider_event = true end)
            end,
        }"#,
                ))
                .unwrap();
            k.set_defs(vec![def]);
            k.start().await.unwrap();
            let mut all = k.open_cursor(std::sync::Arc::new(|_| Some(true)));
            let mut filtered = k.open_cursor(std::sync::Arc::new(|_| Some(true)));
            let lua = k.lua();
            let wire: mlua::Value =
                lua.load(r#"return {headers = {authorization = "synthetic-secret"}}"#).eval().unwrap();
            let rewritten = k.waterfall("provider/request", wire.clone(), mlua::MultiValue::new()).await;
            assert!(rewritten.as_table().unwrap().get::<bool>("rewritten").unwrap());
            let result = k.bail("provider/transport", mlua::MultiValue::from_vec(vec![wire.clone()])).await;
            assert_eq!(result.as_string().unwrap().to_str().unwrap(), "handled");
            k.emit("provider/event", mlua::MultiValue::from_vec(vec![wire])).await;
            assert!(lua.globals().get::<mlua::Table>("mare").unwrap().get::<bool>("_provider_event").unwrap());
            k.emit("status/changed", mlua::MultiValue::new()).await;
            for c in [&mut all, &mut filtered] {
                let events = std::iter::from_fn(|| c.try_recv().ok().flatten())
                    .filter_map(|rec| match rec {
                        mare_kernel::CursorRecord::Event { event, selected } if selected => Some(event.name),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(events, ["status/changed"]);
            }
        })
        .await
}

#[tokio::test]
async fn ordinary_cursors_do_not_drop_or_fail_when_another_cursor_stalls() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            k.set_event_log_limits(mare_kernel::EventLogLimits {
                capacity: 2,
                ..mare_kernel::EventLogLimits::default()
            });
            let mut slow = k.open_cursor(std::sync::Arc::new(|_| Some(true)));
            let mut fast = k.open_cursor(std::sync::Arc::new(|_| Some(true)));
            k.emit_json("item/completed", serde_json::json!({"n": 1})).await;
            k.emit_json("item/completed", serde_json::json!({"n": 2})).await;
            k.emit_json("item/completed", serde_json::json!({"n": 3})).await;
            assert!(slow.failure().borrow().is_none());
            assert!(fast.failure().borrow().is_none());
            let mare_kernel::CursorRecord::Lagged { from_seq: 1, to_seq: 1 } = slow.try_recv().unwrap().unwrap() else {
                panic!("expected Lagged {{1, 1}} for stalled ordinary cursor")
            };
            let mare_kernel::CursorRecord::Event { event: e2, .. } = slow.try_recv().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(e2.payload["n"], 2);
            k.emit(
                "provider/request",
                mlua::MultiValue::from_vec(vec![
                    mare_shared::lua::json_to_lua(k.lua(), &serde_json::json!({"secret": "x"})).unwrap(),
                ]),
            )
            .await;
            assert!(fast.try_recv().unwrap().is_some());
        })
        .await;
}

#[tokio::test]
async fn reliable_tap_captures_the_filter_at_admission() {
    tokio::task::LocalSet::new()
        .run_until(async {
            use std::sync::atomic::{AtomicBool, Ordering};
            let k = kernel();
            let selected = std::sync::Arc::new(AtomicBool::new(true));
            let filter = selected.clone();
            let mut cursor = k.open_cursor(std::sync::Arc::new(move |_| Some(filter.load(Ordering::SeqCst))));
            k.emit_json("test/event", serde_json::json!({"n": 1})).await;
            selected.store(false, Ordering::SeqCst);
            k.emit_json("test/event", serde_json::json!({"n": 2})).await;
            assert!(matches!(cursor.recv().await.unwrap(), mare_kernel::CursorRecord::Event { selected: true, .. }));
            assert!(matches!(cursor.recv().await.unwrap(), mare_kernel::CursorRecord::Event { selected: false, .. }));
        })
        .await;
}

#[tokio::test]
async fn headless_tap_flushes_trailing_dropped_and_respects_event_max_bytes() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            k.set_event_log_limits(mare_kernel::EventLogLimits {
                capacity: 1,
                ..mare_kernel::EventLogLimits::default()
            });
            let (tap, mut rx) = k.open_headless_cursor(std::sync::Arc::new(|name| {
                Some(mare_kernel::HeadlessSelection { selected: true, required: name == "required" })
            }));
            k.emit_json("best-effort", serde_json::json!({"n": 1})).await;
            k.emit_json("best-effort", serde_json::json!({"n": 2})).await;
            let first = rx.recv().await.unwrap();
            let mare_kernel::HeadlessRecord::Event(first_ev) = first.record else { panic!("expected event") };
            tap.flush_lagged().await.unwrap();
            let marker = rx.recv().await.unwrap();
            let mare_kernel::HeadlessRecord::Dropped { count, .. } = marker.record else { panic!("expected dropped") };
            assert_eq!(count, 1);

            // Oversized string payloads are bounded and delivered with `truncated: true`.
            let too_large = "x".repeat(mare_kernel::EVENT_MAX_BYTES + 1);
            k.emit_json("step/completed", serde_json::json!({"turn_id": "t-1", "text": too_large})).await;
            let bounded = rx.try_recv().expect("oversized text event must be truncated and delivered");
            let mare_kernel::HeadlessRecord::Event(bounded_ev) = bounded.record else { panic!("expected event") };
            assert_eq!(bounded_ev.payload["truncated"], true, "{}", bounded_ev.payload);
            assert!(bounded_ev.wire_json.len() <= mare_kernel::EVENT_MAX_BYTES);
            assert_eq!(bounded_ev.seq, first_ev.seq + 1);

            // Irreducibly oversized payloads still record Dropped and consume no seq.
            let irreducible = vec![1u8; mare_kernel::EVENT_MAX_BYTES];
            k.emit_json("step/completed", serde_json::json!({"turn_id": "t-1", "nums": irreducible})).await;
            let drop_marker = rx.try_recv().expect("irreducible oversized event must record Dropped");
            assert!(matches!(drop_marker.record, mare_kernel::HeadlessRecord::Dropped { count: 1, .. }));
            k.emit_json("step/completed", serde_json::json!({"turn_id": "t-1", "text": "ok"})).await;
            let next = rx.try_recv().expect("healthy event after oversized");
            let mare_kernel::HeadlessRecord::Event(next_ev) = next.record else { panic!("expected event") };
            assert_eq!(next_ev.seq, bounded_ev.seq + 1, "oversized event must consume no seq");
        })
        .await;
}

#[test]
fn seq_is_per_session_contiguous_and_shared_by_every_cursor() {
    use crate::common::rpc::{PeerClient, RpcClient};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        r#"mare.setup { peer = { allow = { "events*", "events.subscribe", "events.list", "agent.prompt" } } }"#,
    )
    .unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "hello" }, { "text": "second" }]).to_string()).unwrap();

    let mut env = BTreeMap::new();
    env.insert("MARE_HOME".into(), home.path().display().to_string());
    env.insert("MARE_FAUX_SCRIPT".into(), script.display().to_string());
    let mut rpc = RpcClient::spawn(&["--rpc", "--peer", "--model", "faux/echo", "--no-session"], &env, home.path());
    let init = rpc.call("initialize", json!({"protocol_version": "3.0"}));
    assert!(init.get("error").is_none(), "{init}");
    let init_sid = init["session"]["session_id"].as_str().unwrap().to_owned();
    let _ = rpc.call("events.subscribe", json!({ "scope": "all", "include": ["*"] }));

    let sock = home.path().join("state/peers").join(format!("{}.sock", rpc.pid()));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !sock.exists() && std::time::Instant::now() < deadline {
        // sleep-ok: bounded poll waiting for peer socket creation on disk
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut peer = PeerClient::connect(home.path(), rpc.pid());
    let init_reply = peer.call("initialize", json!({"protocol_version": "3.0"}));
    assert!(init_reply.get("error").is_none(), "peer initialize failed: {init_reply}");
    let sub_reply =
        peer.call("events.subscribe", json!({ "session_id": init_sid, "include": ["turn/started", "turn/completed"] }));
    assert!(sub_reply.get("error").is_none(), "peer subscribe failed: {sub_reply}");

    let r1 = rpc.call("agent.prompt", json!({ "session_id": init_sid, "input": "first" }));
    assert!(r1["status"].as_str() == Some("started") || r1["turn_id"].is_string(), "{r1}");
    while !rpc.events.iter().any(|e| e["type"] == "turn/completed") {
        let msg = rpc.read();
        rpc.dispatch(msg);
    }

    let mut peer_events = Vec::new();
    let mut peer_loss_markers = Vec::new();
    while peer_events.len() < 2 {
        let v = peer.read();
        match v.get("method").and_then(Value::as_str) {
            Some("event") => peer_events.push(v["params"].clone()),
            Some("lagged" | "dropped") => peer_loss_markers.push(v),
            _ => {}
        }
    }

    let p_start = rpc.events.iter().find(|e| e["type"] == "turn/started").unwrap().clone();
    let p_stream = rpc.events.iter().find(|e| e["type"] == "item/delta").unwrap().clone();
    let p_end = rpc.events.iter().find(|e| e["type"] == "turn/completed").unwrap().clone();
    assert!(p_start["session_id"].is_string(), "expected snake_case session_id: {p_start}");
    assert!(p_start["session_epoch"].is_string(), "expected session_epoch: {p_start}");
    assert!(p_start["turn_id"].is_string(), "expected snake_case turn_id: {p_start}");
    assert!(p_start.get("ignorable").is_none(), "required event must omit ignorable: {p_start}");
    assert_eq!(
        p_stream.get("ignorable"),
        Some(&json!(true)),
        "lossy ignorable event must carry ignorable=true: {p_stream}"
    );

    let epoch_1 = p_start["session_epoch"].as_str().unwrap().to_string();
    let seqs: Vec<u64> =
        rpc.events.iter().filter(|e| e["session_epoch"] == epoch_1).map(|e| e["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "primary seq must be contiguous: {seqs:?}");

    assert_eq!(
        peer_events[0]["seq"], p_start["seq"],
        "filtered peer must share seq with primary: peer={peer_events:?} start={p_start}"
    );
    assert_eq!(peer_events[0]["session_epoch"], p_start["session_epoch"]);
    assert_eq!(
        peer_events[1]["seq"], p_end["seq"],
        "filtered peer must share seq with primary: peer={peer_events:?} end={p_end}"
    );
    assert!(
        peer_events[1]["seq"].as_u64().unwrap() > peer_events[0]["seq"].as_u64().unwrap() + 1,
        "filtered peer sees seq gap across filtered events: {peer_events:?}"
    );
    assert!(
        peer_loss_markers.is_empty(),
        "filtered gaps must not produce lagged/dropped markers: {peer_loss_markers:?}"
    );

    rpc.events.clear();
    let created = rpc.call("session.create", json!({}));
    let created_sid = created["session_id"].as_str().unwrap().to_string();
    while !rpc.events.iter().any(|e| e["session_id"] == created_sid && e["type"] == "session/changed") {
        let msg = rpc.read();
        rpc.dispatch(msg);
    }
    let new_epoch = rpc
        .events
        .iter()
        .find(|e| e["session_id"] == created_sid && e["type"] == "session/changed")
        .unwrap()["session_epoch"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(new_epoch, epoch_1);
    let seqs_2: Vec<u64> =
        rpc.events.iter().filter(|e| e["session_epoch"] == new_epoch).map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs_2.first().copied(), Some(1), "new epoch must restart seq at 1: {seqs_2:?}");
    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();
}

#[tokio::test]
async fn oversized_or_unencodable_events_are_refused_without_a_seq_gap() {
    crate::common::local(async {
        let env2 = crate::common::TestEnv::new().await;
        std::fs::create_dir_all(env2.home.path().join("config/plugins")).unwrap();
        std::fs::write(
            env2.home.path().join("config/plugins/refusal_probe.lua"),
            r#"return { name = "refusal_probe", inject = { "bus" }, apply = function(ctx)
                mare._inproc_notices = 0
                ctx:on("host/notice", function() mare._inproc_notices = mare._inproc_notices + 1 end)
                mare._probe_emit = function(name, val)
                    return pcall(function() ctx.bus:emit(name, val) end)
                end
            end }"#,
        )
        .unwrap();
        let h2 = env2.host().user_plugins().no_session().build().await;
        let mut cursor = h2.kernel.open_cursor(std::sync::Arc::new(|name| Some(name == "host/notice")));

        let (ok0, _): (bool, String) = h2
            .kernel
            .lua()
            .load(r#"local ok, e = mare._probe_emit("host/notice", { level = "info", text = "first" }); return ok, tostring(e)"#)
            .eval_async()
            .await
            .unwrap();
        assert!(ok0);

        // Oversized (> 1 MiB) and unencodable events never throw to emitter and still run in-process listeners.
        let (ok1, err1): (bool, String) = h2
            .kernel
            .lua()
            .load(r#"local nums = {}; for i = 1, 600000 do nums[i] = 1 end; local ok, e = mare._probe_emit("host/notice", { level = "info", text = "irreducible", nums = nums }); return ok, tostring(e)"#)
            .eval_async()
            .await
            .unwrap();
        assert!(ok1, "oversized event must never throw to emitter: {err1}");

        let (ok2, err2): (bool, String) = h2
            .kernel
            .lua()
            .load(r#"local ok, e = mare._probe_emit("host/notice", { level = "info", bad = function() end }); return ok, tostring(e)"#)
            .eval_async()
            .await
            .unwrap();
        assert!(ok2, "unencodable event must never throw to emitter: {err2}");

        let (ok3, _): (bool, String) = h2
            .kernel
            .lua()
            .load(r#"local ok, e = mare._probe_emit("host/notice", { level = "info", text = "second" }); return ok, tostring(e)"#)
            .eval_async()
            .await
            .unwrap();
        assert!(ok3);

        let inproc: i64 = h2.kernel.lua().load("return mare._inproc_notices").eval().unwrap();
        assert_eq!(inproc, 4, "in-process listeners must still run for every emitted event");

        let mare_kernel::CursorRecord::Event { event: first, .. } = cursor.try_recv().unwrap().unwrap() else {
            panic!("expected first notice")
        };
        let mare_kernel::CursorRecord::Dropped { count: dropped, .. } = cursor.try_recv().unwrap().unwrap() else {
            panic!("expected Dropped {{ count: 2 }} for refused events")
        };
        assert_eq!(dropped, 2);
        let mare_kernel::CursorRecord::Event { event: second, .. } = cursor.try_recv().unwrap().unwrap() else {
            panic!("expected second notice")
        };
        assert_eq!(first.payload["text"], "first");
        assert_eq!(second.payload["text"], "second");
        assert_eq!(second.seq, first.seq + 1, "refused events must not allocate seq");
        h2.shutdown().await;
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn headless_stream_fails_when_a_required_event_waits_over_the_limit() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            k.set_event_log_limits(mare_kernel::EventLogLimits {
                capacity: 1,
                ..mare_kernel::EventLogLimits::default()
            });
            let (tap, _hcursor) = k.open_headless_cursor(std::sync::Arc::new(|e| match e {
                "step/completed" => Some(mare_kernel::HeadlessSelection { required: true, selected: true }),
                "host/notice" => Some(mare_kernel::HeadlessSelection { required: false, selected: true }),
                _ => None,
            }));
            let failure = tap.failure.clone();
            k.emit_json("step/completed", serde_json::json!({ "turn_id": "t-1", "n": 1 })).await;
            // Lossy event while full must NOT block and must NOT fail the stream.
            k.emit_json("host/notice", serde_json::json!({ "n": 2 })).await;
            assert!(failure.borrow().is_none());

            let k2 = k.clone();
            let prod = tokio::task::spawn_local(async move {
                k2.emit_json("step/completed", serde_json::json!({ "turn_id": "t-1", "n": 3 })).await;
            });
            tokio::task::yield_now().await;
            tokio::time::advance(std::time::Duration::from_secs(6)).await;
            tokio::task::yield_now().await;
            assert!(failure.borrow().is_some(), "stalled headless stream must fail once required event waits > 5s");
            prod.await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn evicted_cursor_gets_lagged_with_seq_range() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            k.set_event_log_limits(mare_kernel::EventLogLimits {
                capacity: 2,
                ..mare_kernel::EventLogLimits::default()
            });
            let mut cursor = k.open_cursor(std::sync::Arc::new(|_| Some(true)));
            let failure = cursor.failure();
            for i in 1..=5 {
                k.emit_json("host/notice", serde_json::json!({ "level": "info", "text": format!("{i}") })).await;
            }
            assert!(
                failure.borrow().is_none(),
                "ordinary cursor must not fail/disconnect on eviction; got {:?}",
                *failure.borrow()
            );
            let lag = cursor.try_recv().unwrap().expect("expected Lagged marker");
            assert!(
                matches!(lag, mare_kernel::CursorRecord::Lagged { from_seq: 1, to_seq: 3 }),
                "expected Lagged {{ from_seq: 1, to_seq: 3 }}, got {lag:?}"
            );
            let e4 = cursor.try_recv().unwrap().expect("expected entry 4");
            assert!(matches!(e4, mare_kernel::CursorRecord::Event { event, .. } if event.seq == 4));
            let e5 = cursor.try_recv().unwrap().expect("expected entry 5");
            assert!(matches!(e5, mare_kernel::CursorRecord::Event { event, .. } if event.seq == 5));
        })
        .await;
}

fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn push_png_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let mut check = Vec::with_capacity(4 + data.len());
    check.extend_from_slice(tag);
    check.extend_from_slice(data);
    out.extend_from_slice(&crc32_ieee(&check).to_be_bytes());
}

fn build_large_png(width: u32, height: u32, min_disk_bytes: usize) -> Vec<u8> {
    // Build a valid RGBA PNG using uncompressed zlib stored blocks so `mare.image.load` decodes it cleanly.
    let stride = 1 + (width as usize) * 3;
    let mut raw = Vec::with_capacity(stride * (height as usize));
    let mut state = 0x1234_5678_9abc_def0u64;
    for _y in 0..height {
        raw.push(0); // filter: None
        for _x in 0..width {
            for _ in 0..3 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                raw.push((state & 0xff) as u8);
            }
        }
    }
    let mut zlib = vec![0x78, 0x01]; // CMF/FLG
    let mut s1: u32 = 1;
    let mut s2: u32 = 0;
    for &b in &raw {
        s1 = (s1 + u32::from(b)) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    for (idx, chunk) in raw.chunks(65535).enumerate() {
        let is_last = (idx + 1) * 65535 >= raw.len();
        zlib.push(u8::from(is_last));
        let len = chunk.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(chunk);
    }
    zlib.extend_from_slice(&((s2 << 16) | s1).to_be_bytes());

    let mut png = vec![137, 80, 78, 71, 13, 10, 26, 10];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
    push_png_chunk(&mut png, b"IHDR", &ihdr);
    push_png_chunk(&mut png, b"IDAT", &zlib);
    if png.len() + 12 < min_disk_bytes {
        let pad_len = min_disk_bytes - png.len() - 12;
        let mut text = Vec::with_capacity(pad_len.max(8));
        text.extend_from_slice(b"pad\0");
        text.resize(text.len() + pad_len.saturating_sub(4), b'A');
        push_png_chunk(&mut png, b"tEXt", &text);
    }
    push_png_chunk(&mut png, b"IEND", &[]);
    png
}

#[tokio::test]
async fn large_png_read_turn_succeeds_with_and_without_stream_consumers() {
    crate::common::local(async {
        let png = build_large_png(650, 650, 1_520_000);
        assert!(png.len() >= 1_500_000, "disk PNG must be >= 1.5MB ({})", png.len());

        // 1. Without any external stream consumer: turn must succeed without event_too_large.
        {
            let env = crate::common::TestEnv::new().await;
            std::fs::write(env.cwd.join("shot.png"), &png).unwrap();
            let script = serde_json::json!([
                { "tool_calls": [{ "name": "read", "arguments": { "path": "shot.png" } }] },
                { "text": "seen" }
            ]);
            let h = env.host().faux(script).no_session().build().await;
            let res = h.prompt("inspect image", serde_json::json!({})).await.unwrap();
            assert!(res["error"].is_null(), "turn without consumers must succeed: {res}");
            assert_eq!(res["text"], "seen", "{res}");
            h.shutdown().await;
        }

        // 2. With an external stream consumer subscribed to public events: turn succeeds, image is
        //    projected to a truncated ref on the outbound tap, and zero `dropped` markers are emitted.
        {
            let env = crate::common::TestEnv::new().await;
            std::fs::write(env.cwd.join("shot.png"), &png).unwrap();
            let script = serde_json::json!([
                { "tool_calls": [{ "name": "read", "arguments": { "path": "shot.png" } }] },
                { "text": "seen" }
            ]);
            let h = env.host().faux(script).no_session().build().await;
            let mut cursor = h
                .kernel
                .open_cursor(std::sync::Arc::new(|name| Some(mare_protocol::event_registry::is_protocol_event(name))));
            let res = h.prompt("inspect image", serde_json::json!({})).await.unwrap();
            assert!(res["error"].is_null(), "turn with consumer must succeed: {res}");
            assert_eq!(res["text"], "seen", "{res}");

            let mut drops = 0u64;
            let mut seqs = Vec::new();
            let mut tool_result_payload = None;
            while let Ok(Some(rec)) = cursor.try_recv() {
                match rec {
                    mare_kernel::CursorRecord::Dropped { count, .. } => drops += count,
                    mare_kernel::CursorRecord::Event { event, selected } => {
                        seqs.push(event.seq);
                        if selected && event.name == "item/completed" && event.payload["item"]["type"] == "tool_call" {
                            tool_result_payload = Some(event.payload);
                        }
                    }
                    mare_kernel::CursorRecord::Lagged { .. } => {}
                }
            }
            let payload =
                tool_result_payload.expect("oversized tool_call item/completed must be projected, not dropped");
            let img = payload["item"]["message"]["content"]
                .as_array()
                .and_then(|arr| arr.iter().find(|b| b["type"] == "image"))
                .expect("projected image reference block");
            assert_eq!(img["mime"], "image/png", "{img}");
            assert_eq!(img["path"], "shot.png", "{img}");
            assert_eq!(img["truncated"], true, "{img}");
            assert!(img.get("data").is_none(), "projected image ref must omit inline data: {img}");
            assert!(img["bytes"].as_u64().unwrap_or(0) >= 1_000_000, "{img}");
            assert_eq!(drops, 0, "projected oversized image must not emit Dropped");
            assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "events must leave no seq gap: {seqs:?}");
            h.shutdown().await;
        }
    })
    .await;
}

#[tokio::test]
async fn session_switch_rotates_epoch_once_even_with_unfiltered_cursor() {
    crate::common::local(async {
        let env = crate::common::TestEnv::new().await;
        let script = serde_json::json!([{ "text": "ok" }]);
        let h = env.host().faux(script).build().await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));

        let _ = h.call_service("session", "create", mlua::MultiValue::new()).await.unwrap();
        let res = h.prompt("after new", serde_json::json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");

        let mut changed_epoch = None;
        let mut end_epoch = None;
        let mut epochs = Vec::new();
        while let Ok(Some(rec)) = cursor.try_recv() {
            if let mare_kernel::CursorRecord::Event { event, .. } = rec {
                epochs.push((event.name.clone(), event.session_epoch.clone(), event.seq));
                if event.name == "session/changed" {
                    changed_epoch = Some(event.session_epoch.clone());
                    assert_eq!(event.seq, 1, "session/changed must start at seq 1: {epochs:?}");
                }
                if event.name == "turn/completed" {
                    end_epoch = Some(event.session_epoch);
                }
            }
        }
        assert!(changed_epoch.is_some(), "missing session/changed: {epochs:?}");
        assert_eq!(
            changed_epoch, end_epoch,
            "session/changed and following turn/* events must share one epoch (no double rotation): {epochs:?}"
        );
        h.shutdown().await;
    })
    .await;
}
