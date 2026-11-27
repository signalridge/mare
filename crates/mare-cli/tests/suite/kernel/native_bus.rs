//! `ctx.bus`: a native plugin's mare events go through the Rust entry —
//! the blocked-turn check, the event record and the taps (the RPC / headless streams, with
//! must_deliver backpressure) — and then the bus, legacy value-passing listeners included.

use crate::common::*;
use std::collections::BTreeMap;

fn write_plugin(home: &std::path::Path, name: &str, src: &str) {
    let dir = home.join("config/plugins");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.lua")), src).unwrap();
}

/// A native plugin; the test calls `ctx.bus` through it (`mare._nat_bus(mode, ...)`).
const NATIVE: &str = r#"return {
  name = "nat",
  inject = { "bus" },
  contracts = {
    probe = {
      owner = "nat",
      events = {
        wf = { doc = "probe", mode = "waterfall", expose = "process" },
        ["wf-x1"] = { doc = "probe", mode = "waterfall", expose = "process" },
        ["wf-fail"] = { doc = "probe", mode = "waterfall", expose = "process" },
        quiet = { doc = "probe", mode = "emit", expose = "lua" },
        ["quiet-wf"] = { doc = "probe", mode = "waterfall", expose = "process" },
        live = { doc = "probe", mode = "emit", expose = "lua" },
        ["live-headless"] = { doc = "probe", mode = "emit", expose = "lua" },
        ["stream-a"] = { doc = "probe", mode = "emit", expose = "process" },
        ["stream-b"] = { doc = "probe", mode = "waterfall", expose = "process" },
      },
    },
  },
  apply = function(ctx)
    mare._nat_bus = function(mode, ...) return ctx.bus[mode](ctx.bus, ...) end
    -- around form: passes its value on
    ctx:on("probe/wf", function(v, next) return next(v .. "N") end)
  end,
}"#;

/// A public event a native plugin emits through ctx.bus reaches an RPC client's event stream.
#[test]
fn a_native_bus_emit_of_a_public_event_reaches_the_rpc_client() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    write_plugin(
        &home,
        "nat",
        r#"return { name = "nat", inject = { "bus" }, apply = function(ctx)
  -- turn/started comes from the agent through the bus to this native listener
  ctx:on("turn/started", function()
    ctx.bus:emit("host/notice", { level = "info", text = "from the native bus" })
  end)
end }"#,
    );
    let env = BTreeMap::from([("MARE_HOME".to_string(), home.display().to_string())]);
    let mut c =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo", "--no-session"], &env, &work);
    let init = c.call("initialize", serde_json::json!({ "protocol_version": "3.0" }));
    let sid = init["session"]["session_id"].as_str().unwrap().to_string();
    let p = c.call("agent.prompt", serde_json::json!({ "session_id": sid, "input": "go" }));
    c.call("agent.wait", serde_json::json!({ "session_id": sid, "turn_id": p["turn_id"] }));
    let notice = loop {
        let e = c.wait_event("host/notice");
        if e["data"]["text"] == "from the native bus" {
            break e;
        }
    };
    assert_eq!(notice["data"]["level"], "info", "{notice}");
    c.shutdown_and_wait();
}

/// A blocked turn refuses a frontend turn event emitted through ctx.bus (the Rust entry's check);
/// nothing starts.
#[tokio::test]
async fn a_native_bus_turn_event_is_refused_while_turns_are_blocked() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(env.home.path(), "nat", NATIVE);
        let h = mare_host::Host::new(env.host().user_plugins().no_session().faux(serde_json::json!([])).opts)
            .await
            .unwrap();
        expose(&h);
        exec(&h, "STARTED = false C.cordis:on('turn/started', function() STARTED = true end)").await;
        mare_shared::session_barrier::barrier(h.lua()).poison("the session is being replaced");
        let (ok, err, started): (bool, String, bool) = eval(
            &h,
            r#"local ok, err = pcall(function() return C.cordis:get("agent"):prompt("hi") end)
               return ok, tostring(err), STARTED"#,
        )
        .await;
        assert!(!ok && err.contains("the session is being replaced"), "{err}");
        assert!(!started, "a turn started");
        h.shutdown().await;
    })
    .await
}

/// A required event emitted through ctx.bus waits for room in a full fixed-cursor stream
/// (headless backpressure), and goes on once the cursor drains.
#[tokio::test]
async fn a_native_bus_emit_of_a_must_deliver_event_waits_for_a_full_tap() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(env.home.path(), "nat", NATIVE);
        let h = mare_host::Host::new(env.host().user_plugins().no_session().opts).await.unwrap();
        expose(&h);
        h.kernel.set_event_log_limits(mare_kernel::EventLogLimits {
            capacity: 1,
            ..mare_kernel::EventLogLimits::default()
        });
        let (_tap, mut hcursor) = h.kernel.open_headless_cursor(std::sync::Arc::new(|name| {
            Some(mare_kernel::HeadlessSelection {
                required: mare_protocol::event_registry::delivery_of(name)
                    == Some(mare_protocol::event_registry::EventDelivery::Required),
                selected: true,
            })
        }));
        // fill the capacity-1 event log
        h.kernel.emit_json("host/notice", serde_json::json!({ "level": "info", "text": "filler" })).await;
        let lua = h.lua().clone();
        let emitted = tokio::task::spawn_local(async move {
            lua.load(
                r#"mare._nat_bus("emit", "turn/completed", { turn_id = "t-1", status = "completed" }) return true"#,
            )
            .eval_async::<bool>()
            .await
            .unwrap()
        });
        // sleep-ok: negative window: a required emit is still pending after 200 ms while the fixed cursor blocks
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!emitted.is_finished(), "a must_deliver emit did not wait for the full tap");
        let first = hcursor.recv().await.unwrap();
        let mare_kernel::HeadlessRecord::Event(first) = first.record else { panic!("expected event") };
        assert_eq!(first.name, "host/notice");
        assert!(emitted.await.unwrap());
        let second = hcursor.recv().await.unwrap();
        let mare_kernel::HeadlessRecord::Event(second) = second.record else { panic!("expected event") };
        assert_eq!(second.name, "turn/completed");
        h.shutdown().await;
    })
    .await
}

/// on the native bus: a listener reached through another one's `next` runs in the context it
/// registered in, not in its caller's. An unrestricted listener put first calls `next`; the one after
/// it registered inside a restricted tool context (a deny_read), so its read is still denied — the
/// chain hands on neither the first listener's rights nor the emitter's.
#[tokio::test]
async fn a_listener_reached_through_next_keeps_its_own_registration_context() {
    local(async {
        let env = TestEnv::new().await;
        install_probe_events_plugin(&env, "probe", &[("wf-x1", "waterfall")]);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            local sid = S:info().session_id
            ITEM = mare.spill.save(sid, "secret")
            C.cordis:on("probe/wf-x1", function(value, next) return next(value .. "N") end, { prepend = true })
            INNER = {}
            local restricted = {sandbox={level="read-only",network=true,
                deny_read={mare.paths.state .. "/spill-results"}, accept_partial={"fs_deny_read"}}}
            assert(mare.exec.run(restricted, function()
                C.cordis:on("probe/wf-x1", function(value, next)
                    INNER[#INNER + 1] = pcall(mare.spill.read, ITEM.spill_id)
                    return next(value .. "L")
                end)
                return true
            end))
            OUT = C.cordis.bus:waterfall("probe/wf-x1", "s")
            OUT_CHECKED = C.cordis.bus:waterfall_checked("probe/wf-x1", "s")
            "#,
        )
        .await;
        let (out, checked, inner): (String, String, Vec<bool>) = eval(&h, "return OUT, OUT_CHECKED, INNER").await;
        h.shutdown().await;
        assert_eq!((out.as_str(), checked.as_str()), ("sNL", "sNL"), "the unrestricted one first");
        assert_eq!(inner, [false, false], "the listener after next read what its registrant may not: {inner:?}");
    })
    .await
}

/// An unchecked waterfall keeps going past a failing listener, with the value it was handed (the
/// old engine's waterfall); a checked one stops at it.
#[tokio::test]
async fn an_unchecked_waterfall_skips_a_failing_listener_and_keeps_the_value() {
    local(async {
        let env = TestEnv::new().await;
        install_probe_events_plugin(&env, "probe", &[("wf-fail", "waterfall")]);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            C.cordis:on("probe/wf-fail", function(v, next) return next(v .. "a") end)
            C.cordis:on("probe/wf-fail", function() error("boom") end)
            C.cordis:on("probe/wf-fail", function(v, next) return next(v .. "c") end)
            "#,
        )
        .await;
        let (out, checked): (String, bool) = eval(
            &h,
            r#"local bus = C.cordis.bus
            return bus:waterfall("probe/wf-fail", "s"),
              pcall(bus.waterfall_checked, bus, "probe/wf-fail", "s")"#,
        )
        .await;
        h.shutdown().await;
        assert_eq!(out, "sac", "the rest of the chain's value");
        assert!(!checked, "a checked waterfall stops at the failing listener");
    })
    .await
}

/// An event nobody listens to and no stream takes costs nothing: ctx.bus:emit / waterfall return
/// at once (an emit nothing, a waterfall its value unchanged) without entering Rust — not recorded
/// (`recent_events`), not tapped — so a plugin emits unconditionally, a stream chunk included
/// (provider/event). A tap added later that takes the event gets it from then on: the bus's
/// per-event answer is forgotten when a tap is added.
#[tokio::test]
async fn a_native_bus_emit_to_nobody_skips_the_rust_entry_until_a_tap_takes_the_event() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(env.home.path(), "nat", NATIVE);
        let h = mare_host::Host::new(env.host().user_plugins().no_session().opts).await.unwrap();
        expose(&h);
        let value: String = eval(
            &h,
            r#"mare._nat_bus("emit", "probe/quiet", { n = 1 })
               return mare._nat_bus("waterfall", "probe/quiet-wf", "v")"#,
        )
        .await;
        assert_eq!(value, "v", "a waterfall to nobody returns its value");
        let recent = mare_kernel::recent_events();
        assert!(
            !recent.iter().any(|e| e == "probe/quiet" || e == "probe/quiet-wf"),
            "an event to nobody entered the Rust entry: {recent:?}"
        );
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|e| (e == "probe/quiet").then_some(true)));
        exec(&h, r#"mare._nat_bus("emit", "probe/quiet", { n = 2 })"#).await;
        let Some(mare_kernel::CursorRecord::Event { event: ev, .. }) = cursor.try_recv().unwrap() else {
            panic!("a tap added later takes the event");
        };
        assert_eq!(ev.name, "probe/quiet");
        assert_eq!(ev.payload["n"], 2);
        // a listener on the tree gets it too, the tap still does
        exec(&h, r#"GOT = nil C.cordis:on("probe/quiet", function(ev) GOT = ev.n end) mare._nat_bus("emit", "probe/quiet", { n = 3 })"#)
            .await;
        let got: i64 = eval(&h, "return GOT").await;
        assert_eq!(got, 3);
        let Some(mare_kernel::CursorRecord::Event { event: ev3, .. }) = cursor.try_recv().unwrap() else {
            panic!("expected probe/quiet n=3 event");
        };
        assert_eq!(ev3.payload["n"], 3);
        h.shutdown().await;
    })
    .await
}

/// A tap whose filter reads live state (an RPC connection's subscription) is asked on every
/// emit, never answered from a cache: an event it did not take before is delivered once it does.
#[tokio::test]
async fn a_native_bus_emit_follows_a_live_tap_filter() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(env.home.path(), "nat", NATIVE);
        let h = mare_host::Host::new(env.host().user_plugins().no_session().opts).await.unwrap();
        expose(&h);
        let subscribed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let live = subscribed.clone();
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(move |name| {
            (name == "probe/live" && live.load(std::sync::atomic::Ordering::SeqCst)).then_some(true)
        }));
        exec(&h, r#"mare._nat_bus("emit", "probe/live", { n = 1 })"#).await;
        assert!(cursor.try_recv().unwrap().is_none(), "not subscribed yet");
        subscribed.store(true, std::sync::atomic::Ordering::SeqCst);
        exec(&h, r#"mare._nat_bus("emit", "probe/live", { n = 2 })"#).await;
        let Some(mare_kernel::CursorRecord::Event { event: ev, .. }) = cursor.try_recv().unwrap() else {
            panic!("expected event delivered once the live filter takes it");
        };
        assert_eq!(ev.name, "probe/live");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_native_bus_emit_follows_a_live_headless_tap_filter() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(env.home.path(), "nat", NATIVE);
        let h = mare_host::Host::new(env.host().user_plugins().no_session().opts).await.unwrap();
        expose(&h);
        let subscribed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let live = subscribed.clone();
        let (_tap, mut rx) = h.kernel.open_headless_cursor(std::sync::Arc::new(move |name| {
            (name == "probe/live-headless" && live.load(std::sync::atomic::Ordering::SeqCst))
                .then_some(mare_kernel::HeadlessSelection { selected: true, required: false })
        }));
        exec(&h, r#"mare._nat_bus("emit", "probe/live-headless", { n = 1 })"#).await;
        assert!(rx.try_recv().is_none(), "not subscribed yet");
        subscribed.store(true, std::sync::atomic::Ordering::SeqCst);
        exec(&h, r#"mare._nat_bus("emit", "probe/live-headless", { n = 2 })"#).await;
        let ev = rx.try_recv().expect("delivered once the live headless filter takes it");
        let mare_kernel::HeadlessRecord::Event(ev) = ev.record else { panic!("expected event") };
        assert_eq!(ev.name, "probe/live-headless");
        assert_eq!(ev.payload["n"], 2);
        h.shutdown().await;
    })
    .await
}

/// A listener of every mare event (`internal/dispatch`, cordis' full stream) gets events nobody
/// listens to by name: two such events, emitted through ctx.bus, both reach it, so the bus's
/// quiet return never skips them (ported from Kimi Code
/// packages/agent-core-v2/test/app/event/eventBus.test.ts:46, "delivers every published event to a
/// full-stream subscriber"). The bus service shows no member besides its emit methods.
#[tokio::test]
async fn a_full_stream_listener_gets_events_nobody_listens_to_by_name() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(env.home.path(), "nat", NATIVE);
        write_plugin(
            env.home.path(),
            "stream",
            r#"return { name = "stream", apply = function(ctx)
  mare._seen = {}
  ctx:on("internal/dispatch", function(_, name)
    if name:sub(1, 6) == "probe/" then mare._seen[#mare._seen + 1] = name end
  end)
end }"#,
        );
        let h = mare_host::Host::new(env.host().user_plugins().no_session().opts).await.unwrap();
        expose(&h);
        let seen: String = eval(
            &h,
            r#"mare._nat_bus("emit", "probe/stream-a", { x = 1 })
               mare._nat_bus("waterfall", "probe/stream-b", "y")
               return table.concat(mare._seen, ",")"#,
        )
        .await;
        assert_eq!(seen, "probe/stream-a,probe/stream-b", "both events reach the full-stream listener");
        h.shutdown().await;
    })
    .await
}

/// The bus service a native plugin reaches shows nothing but its emit methods: the quiet check and
/// the Rust entry stay internal (no listener query).
#[tokio::test]
async fn the_bus_service_shows_plugins_only_its_emit_methods() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(
            env.home.path(),
            "peek",
            r#"return { name = "peek", inject = { "bus" }, apply = function(ctx)
  mare._peek = { quiet = type(ctx.bus.quiet), dispatch = type(ctx.bus.dispatch), emit = ctx.bus.emit ~= nil }
end }"#,
        );
        let h = mare_host::Host::new(env.host().user_plugins().no_session().opts).await.unwrap();
        let (quiet, dispatch, emit): (String, String, bool) =
            eval(&h, "return mare._peek.quiet, mare._peek.dispatch, mare._peek.emit").await;
        assert_eq!((quiet.as_str(), dispatch.as_str(), emit), ("nil", "nil", true));
        h.shutdown().await;
    })
    .await
}
