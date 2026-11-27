use super::{Env, Kernel, Mode, UiShared, kernel, lua, src};

#[tokio::test]
async fn waterfall_checked_stops_on_listener_error() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let err_def = k
                .load_def(&src(
                    "fail_wf",
                    r#"return {
                        name = "fail_wf",
                        contracts = {
                            ok = { owner = "fail_wf", events = { wf = { doc = "ok wf", mode = "waterfall", expose = "process" } } },
                            fail = { owner = "fail_wf", events = { wf = { doc = "fail wf", mode = "waterfall", expose = "process" } } },
                        },
                        apply = function(ctx)
                            ctx:on("ok/wf", function(v, next) return next(v .. "+ok") end)
                            ctx:on("fail/wf", function() error("boom") end)
                        end
                    }"#,
                ))
                .unwrap();
            k.set_defs(vec![err_def]);
            k.start().await.unwrap();
            let lua = k.lua().clone();
            let ok = k
                .waterfall_checked(
                    "ok/wf",
                    mlua::Value::String(lua.create_string("x").unwrap()),
                    mlua::MultiValue::new(),
                )
                .await
                .unwrap();
            assert_eq!(ok.as_string().unwrap().to_str().unwrap(), "x+ok");
            let open = k
                .waterfall("fail/wf", mlua::Value::String(lua.create_string("orig").unwrap()), mlua::MultiValue::new())
                .await;
            assert_eq!(open.as_string().unwrap().to_str().unwrap(), "orig");
            assert!(
                k.waterfall_checked(
                    "fail/wf",
                    mlua::Value::String(lua.create_string("orig").unwrap()),
                    mlua::MultiValue::new()
                )
                .await
                .is_err()
            );
            k.shutdown().await;
        })
        .await
}

#[tokio::test(start_paused = true)]
async fn once_listeners_timers_and_task_errors_reach_the_ui() {
    tokio::task::LocalSet::new().run_until(once_timers_inner()).await
}

async fn once_timers_inner() {
    let k = kernel();
    let def = k
        .load_def(&src(
            "t",
            r#"return { name = "t", inject = { "task" }, apply = function(ctx)
              mare._once, mare._every, mare._errors = 0, 0, {}
              ctx:once("ping", function() mare._once = mare._once + 1 end)
              ctx:after(5, function() mare._after = true end)
              ctx:every(5, function() mare._every = mare._every + 1; return mare._every < 3 end)
              ctx:on("kernel/error", function(ev) table.insert(mare._errors, ev.message) end)
              ctx:spawn(function() error("task boom") end)
              ctx:after(1, function() error("timer boom") end)
              ctx:effect(function() return function() error("dispose boom") end end)
            end }"#,
        ))
        .unwrap();
    k.set_defs(vec![def]);
    k.start().await.unwrap();
    for _ in 0..2 {
        k.emit("ping", mlua::MultiValue::new()).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let (once, after, every): (i64, bool, i64) =
        k.lua().load("return mare._once, mare._after == true, mare._every").eval().unwrap();
    assert_eq!((once, after, every), (1, true, 3));
    let errors: Vec<String> = k.lua().load("return mare._errors").eval().unwrap();
    let all = errors.join("\n");
    assert!(all.contains("task boom") && all.contains("timer boom"), "{all}");
    // a dispose hook's error is shown too (the listener is gone by then: count the event)
    let seen = std::rc::Rc::new(std::cell::Cell::new(false));
    let s2 = seen.clone();
    let (ctx, owner) = (k.host_ctx().unwrap(), k.host_owner().unwrap());
    let f = k
        .lua()
        .create_function(move |_, ev: mlua::Table| {
            if ev.get::<String>("message")?.contains("dispose boom") {
                s2.set(true);
            }
            Ok(())
        })
        .unwrap();
    k.host_on(&ctx, owner, "kernel/error", f, mare_kernel::HostOn::default()).unwrap();
    crate::common::unload_plugin(&k, "t").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(seen.get());
}

/// The timers a schedule plugin builds on: the handle cancels
/// one, and unloading the plugin stops the rest.
#[tokio::test(start_paused = true)]
async fn timers_stop_when_aborted_or_when_their_plugin_is_disposed() {
    tokio::task::LocalSet::new().run_until(timers_stop_inner()).await
}

async fn timers_stop_inner() {
    let k = kernel();
    let def = k
        .load_def(&src(
            "sched",
            r#"return { name = "sched", inject = { "task" }, apply = function(ctx)
              mare._a, mare._b = 0, 0
              local a = ctx:every(2, function() mare._a = mare._a + 1 end)
              ctx:every(2, function() mare._b = mare._b + 1 end)
              mare._stop_a = function() a:abort() end
            end }"#,
        ))
        .unwrap();
    k.set_defs(vec![def]);
    k.start().await.unwrap();
    fn count(k: &Kernel) -> (i64, i64) {
        k.lua().load("return mare._a, mare._b").eval().unwrap()
    }
    async fn wait_until(k: &Kernel, f: impl Fn((i64, i64)) -> bool) {
        let end = tokio::time::Instant::now() + std::time::Duration::from_secs(crate::common::WAIT_SECS);
        while !f(count(k)) {
            assert!(tokio::time::Instant::now() < end, "timers stuck at {:?}", count(k));
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    wait_until(&k, |(a, b)| a >= 2 && b >= 2).await;
    k.lua().load("mare._stop_a()").exec().unwrap();
    let (a, b0) = count(&k);
    // the other timer keeps going meanwhile
    wait_until(&k, |(_, b)| b >= b0 + 3).await;
    assert_eq!(count(&k).0, a, "an aborted timer never fires again");
    crate::common::unload_plugin(&k, "sched").await;
    let (_, b) = count(&k);
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(count(&k), (a, b), "disposing the plugin stops its timers");
}

#[tokio::test(start_paused = true)]
async fn a_policy_listener_without_timeout_ms_gets_the_default_and_denies() {
    tokio::task::LocalSet::new().run_until(policy_default_inner()).await
}

async fn policy_default_inner() {
    let l = lua::new_lua().unwrap();
    let env = Env {
        cwd: std::env::current_dir().unwrap(),
        mode: Mode::Child,
        ui: UiShared::new(),
        config: Default::default(),
        args: serde_json::Value::Null,
    };
    lua::install(&l, &env).unwrap();
    let k = Kernel::new(l, serde_json::json!({ "policy_timeout_ms": 200 })).unwrap();
    let contracts: mlua::Table = k
        .lua()
        .load(r#"return { agent = { methods = {}, events = { ["step-ready"] = { mode = "policy" } } } }"#)
        .eval()
        .unwrap();
    k.install_service_contracts(contracts).unwrap();
    let def = k
        .load_def(&src(
            "slow",
            r#"return { name = "slow", inject = { "policy" }, apply = function(ctx)
              ctx.policy:voter("agent/step-ready", function() mare.task.sleep(60000); return "allow" end)
            end }"#,
        ))
        .unwrap();
    k.set_defs(vec![def]);
    k.start().await.unwrap();
    let t0 = tokio::time::Instant::now();
    let v = k.policy("agent/step-ready", mlua::MultiValue::new()).await.unwrap();
    assert_eq!(v.get::<String>("decision").unwrap(), "deny");
    assert!(v.get::<String>("reason").unwrap().contains("timed out after 200ms"));
    assert!(t0.elapsed() < std::time::Duration::from_secs(5), "{:?}", t0.elapsed());
    assert_eq!(mare_kernel::DEFAULT_POLICY_TIMEOUT, std::time::Duration::from_secs(30));
}

#[tokio::test]
async fn the_watchdog_abort_is_not_swallowed_by_pcall() {
    tokio::task::LocalSet::new().run_until(watchdog_inner()).await
}

async fn watchdog_inner() {
    let k = kernel();
    let _w = mare_kernel::install_watchdog(k.lua(), std::time::Duration::from_millis(200)).unwrap();
    // let the tick task run once, then spin inside a loop of pcalls
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let f: mlua::Function =
        k.lua().load("return function() while true do pcall(function() while true do end end) end end").eval().unwrap();
    let t0 = std::time::Instant::now();
    let e = f.call_async::<()>(()).await.unwrap_err().to_string();
    assert!(e.contains("watchdog"), "{e}");
    assert!(t0.elapsed() < std::time::Duration::from_secs(10), "{:?}", t0.elapsed());
    // a single pcall around a runaway loop doesn't let the callback go on either
    let g: mlua::Function =
        k.lua().load("return function() pcall(function() while true do end end); return 'went on' end").eval().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(g.call_async::<String>(()).await.is_err());
    // after the runtime made progress, Lua runs normally again
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let ok: i64 = k.lua().load("local n = 0 for i = 1, 1000 do n = n + i end return n").eval_async().await.unwrap();
    assert_eq!(ok, 500500);
}

/// A process-only event (`expose = "process"`) declared by a plugin reaches every listener once
/// and no tap — not even one that wants every event — whatever is emitted (a function-valued table included).
#[tokio::test]
async fn process_only_events_reach_every_listener_once_and_no_tap() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let def = k
                .load_def(&src(
                    "channels",
                    r#"return {
                        name = "channels",
                        contracts = {
                            channels = {
                                methods = {},
                                events = {
                                    shared = {
                                        doc = "In-process test event carrying Lua functions.",
                                        mode = "emit",
                                        expose = "process",
                                    },
                                },
                            },
                        },
                        apply = function(ctx)
                            mare._local_count, mare._public_count = 0, 0
                            ctx:on("channels/shared", function(value)
                                assert(type(value.callback) == "function")
                                mare._local_count = mare._local_count + 1
                            end)
                            ctx:on("channels/shared", function(value)
                                assert(type(value.callback) == "function")
                                mare._public_count = mare._public_count + 1
                            end)
                            ctx:emit("channels/shared", { callback = function() end })
                        end,
                    }"#,
                ))
                .unwrap();
            k.set_defs(vec![def]);
            k.start().await.unwrap();
            let mut cursor = k.open_cursor(std::sync::Arc::new(|_| Some(true)));
            let value = k.lua().create_table().unwrap();
            value.set("callback", k.lua().create_function(|_, ()| Ok(())).unwrap()).unwrap();
            k.emit("channels/shared", mlua::MultiValue::from_vec(vec![mlua::Value::Table(value)])).await;
            assert!(cursor.try_recv().unwrap().is_none());
            assert!(cursor.failure().borrow().is_none());
            let counts: (i64, i64) = k.lua().load("return mare._local_count, mare._public_count").eval().unwrap();
            assert_eq!(counts, (2, 2), "each listener once per emit (the apply's emit and this one)");
        })
        .await;
}

/// Every dispatch mode's listener calls are observable without per-call hook/* (gone on the cordis
/// tree, CK-E A): each is timed in mare.profile (slow_callback_ms = 0 times every call), and a
/// failing listener is reported in-process through `kernel/error` with its plugin and event.
#[tokio::test]
async fn listener_calls_of_every_dispatch_mode_are_timed_and_failures_reported() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let l = lua::new_lua().unwrap();
            let env = Env {
                cwd: std::env::current_dir().unwrap(),
                mode: Mode::Child,
                ui: UiShared::new(),
                config: Default::default(),
                args: serde_json::Value::Null,
            };
            lua::install(&l, &env).unwrap();
            let k = Kernel::new(l, serde_json::json!({ "slow_callback_ms": 0 })).unwrap();
            let contracts: mlua::Table = k
                .lua()
                .load(
                    r#"return {
                        agent = { methods = {}, events = { ["step-ready"] = { mode = "policy" } } },
                        tools = { methods = {} },
                        tool = { owner = "tools", events = { rewrite = { mode = "rewrite" } } },
                    }"#,
                )
                .eval()
                .unwrap();
            k.install_service_contracts(contracts).unwrap();
            let def = k
                .load_def(&src(
                    "hook_probe",
                    r#"return { name = "hook_probe", inject = { "policy" }, apply = function(ctx)
            mare._errors = {}
            ctx:on("kernel/error", function(ev) table.insert(mare._errors, ev.plugin .. " " .. ev.event) end)
            ctx:on("test/emit-ok", function() end)
            ctx:on("test/emit-err", function() error("boom") end)
            ctx:on("test/bail", function() return { handled = true } end)
            ctx:on("test/waterfall", function(v, next) return next(v) end)
            ctx.policy:voter("agent/step-ready", function() return { decision = "deny", reason = "nope" } end)
            ctx.policy:rewriter("tool/rewrite", function(c) return c.arguments end)
        end }"#,
                ))
                .unwrap();
            k.set_defs(vec![def]);
            k.start().await.unwrap();

            k.emit_json("test/emit-ok", serde_json::json!({})).await;
            k.emit_json("test/emit-err", serde_json::json!({})).await;
            let bailed = k.bail_json("test/bail", serde_json::json!({})).await;
            let _ = k.waterfall("test/waterfall", mlua::Value::Integer(1), mlua::MultiValue::new()).await;
            let _ = k.policy("agent/step-ready", mlua::MultiValue::new()).await.unwrap();
            let call = k.lua().create_table().unwrap();
            call.set("arguments", k.lua().create_table().unwrap()).unwrap();
            let _ = k.rewrite("tool/rewrite", call).await.unwrap();
            tokio::task::yield_now().await;
            assert_eq!(bailed.as_ref().map(|v| v["handled"].clone()), Some(serde_json::json!(true)), "{bailed:?}");

            let timed: Vec<(String, String, i64)> = k
                .lua()
                .load("return mare.profile.callback_timings()")
                .eval::<Vec<mlua::Table>>()
                .unwrap()
                .into_iter()
                .map(|r| (r.get("plugin").unwrap(), r.get("event").unwrap(), r.get("count").unwrap()))
                .collect();
            for event in
                ["test/emit-ok", "test/emit-err", "test/bail", "test/waterfall", "agent/step-ready", "tool/rewrite"]
            {
                assert!(
                    timed.iter().any(|(p, e, n)| p == "hook_probe" && e == event && *n == 1),
                    "{event}: the listener call was not timed once: {timed:?}"
                );
            }
            let errors: Vec<String> = k.lua().load("return mare._errors").eval().unwrap();
            assert_eq!(errors, ["hook_probe test/emit-err"], "the failing listener is reported in-process");
        })
        .await;
}

/// A listener's raw failure (secrets in its message, the traceback) reaches no public event: it is
/// reported in-process (`kernel/error`, not public), and the public stream carries no copy of it.
/// Before CK-E A this was checked on hook/end; that event goes, the guarantee stays.
#[tokio::test]
async fn a_listener_failure_reaches_no_public_event() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let def = k
                .load_def(&src(
                    "hook_leak_probe",
                    r#"return { name = "hook_leak_probe", apply = function(ctx)
            mare._seen = {}
            ctx:on("kernel/error", function(ev) table.insert(mare._seen, ev.event) end)
            ctx:on("test/leak", function()
                error("INTERNAL_LISTENER_SECRET: token=secret-123\nstack traceback:\n\tinternal.lua:42")
            end)
        end }"#,
                ))
                .unwrap();
            k.set_defs(vec![def]);
            k.start().await.unwrap();
            let mut cursor = k.open_cursor(std::sync::Arc::new(|e| {
                mare_protocol::event_registry::is_protocol_event(e).then_some(true)
            }));
            k.emit_json("test/leak", serde_json::json!({})).await;
            tokio::task::yield_now().await;
            assert!(cursor.failure().borrow().is_none());
            let seen: Vec<String> = k.lua().load("return mare._seen").eval().unwrap();
            assert_eq!(seen, ["test/leak"], "the failure is reported in-process");
            while let Ok(Some(rec)) = cursor.try_recv() {
                let mare_kernel::CursorRecord::Event { event, .. } = rec else { continue };
                let text = event.payload.to_string();
                assert!(
                    !text.contains("INTERNAL_LISTENER_SECRET")
                        && !text.contains("secret-123")
                        && !text.contains("stack traceback"),
                    "raw listener failure leaked into public {}: {text}",
                    event.name
                );
            }
        })
        .await;
}

#[tokio::test]
async fn ordinary_tap_reports_conversion_error_instead_of_emitting_null() {
    crate::common::local(async {
        let lua = mlua::Lua::new();
        let k = Kernel::new(lua, serde_json::json!({})).unwrap();
        let mut cursor = k.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let bad = k.lua().create_string(b"\xff").unwrap();
        let mut args = mlua::MultiValue::new();
        args.push_back(mlua::Value::String(bad));
        k.emit("test/bad-tap", args).await;
        assert!(
            matches!(cursor.try_recv().unwrap(), Some(mare_kernel::CursorRecord::Dropped { count: 1, .. })),
            "unencodable event must emit Dropped instead of null payload"
        );
        assert!(cursor.try_recv().unwrap().is_none());
        let problems = k.problems();
        assert!(
            problems.iter().any(|p| p.contains("event tap for `test/bad-tap`")),
            "expected tap conversion error in problems: {problems:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn ordinary_tap_rejects_function_userdata_and_cyclic_tables() {
    crate::common::local(async {
        let lua = mlua::Lua::new();
        let k = Kernel::new(lua, serde_json::json!({})).unwrap();
        let mut cursor = k.open_cursor(std::sync::Arc::new(|_| Some(true)));

        for (event, expr) in [
            ("test/fn-tap", "return { f = function() end }"),
            ("test/ud-tap", "return { tok = mare_test_ctx }"),
            ("test/cycle-tap", "local t = {}; t.self = t; return t"),
        ] {
            k.lua()
                .globals()
                .set("mare_test_ctx", k.lua().load("return io.stdout").eval::<mlua::Value>().unwrap())
                .unwrap();
            let v: mlua::Value = k.lua().load(expr).eval().unwrap();
            let mut args = mlua::MultiValue::new();
            args.push_back(v);
            k.emit(event, args).await;
            assert!(
                matches!(cursor.try_recv().unwrap(), Some(mare_kernel::CursorRecord::Dropped { count: 1, .. })),
                "{event} must emit Dropped instead of degraded payload"
            );
            assert!(cursor.try_recv().unwrap().is_none(), "{event} must not emit event payload");
            let expected = format!("event tap for `{event}`");
            let problems = k.problems();
            assert!(
                problems.iter().any(|p| p.contains(&expected)),
                "expected `{expected}` in problems for {event}: {problems:?}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn ordinary_tap_projects_request_prepare_cancel_token_and_real_prompt_has_no_kernel_error() {
    crate::common::local(async {
        let lua = mlua::Lua::new();
        let k = Kernel::new(lua, serde_json::json!({})).unwrap();
        let contracts: mlua::Table = k
            .lua()
            .load(
                r#"return {
                    agent = { methods = {} },
                    request = {
                        owner = "agent",
                        events = {
                            prepare = { doc = "prepare", mode = "serial", expose = "lua" },
                        },
                    },
                }"#,
            )
            .eval()
            .unwrap();
        k.install_service_contracts(contracts).unwrap();
        let mut cursor = k.open_cursor(std::sync::Arc::new(|_| Some(true)));

        // 1. `request/prepare` with a Task Token `cancel` field projects `cancel` away and succeeds.
        let token = mare_binding::task::Token(tokio_util::sync::CancellationToken::new());
        k.lua().globals().set("tap_test_token", token).unwrap();
        let valid: mlua::Value = k.lua().load(r#"return { model = "m", cancel = tap_test_token }"#).eval().unwrap();
        let mut args = mlua::MultiValue::new();
        args.push_back(valid);
        k.emit_checked("request/prepare", args).await.unwrap();
        let Some(mare_kernel::CursorRecord::Event { event: ev, .. }) = cursor.try_recv().unwrap() else {
            panic!("expected request/prepare tap event");
        };
        assert_eq!(ev.name, "request/prepare");
        assert_eq!(ev.payload, serde_json::json!({ "model": "m" }));
        assert!(k.problems().is_empty(), "unexpected problems: {:?}", k.problems());

        // 2. Non-token userdata in `request/prepare.cancel` is still rejected and records a refusal drop.
        k.lua().globals().set("tap_test_ctx", k.lua().load("return io.stdout").eval::<mlua::Value>().unwrap()).unwrap();
        let invalid: mlua::Value = k.lua().load(r#"return { model = "m", cancel = tap_test_ctx }"#).eval().unwrap();
        let mut bad_args = mlua::MultiValue::new();
        bad_args.push_back(invalid);
        k.emit_checked("request/prepare", bad_args).await.unwrap();
        assert!(
            matches!(cursor.try_recv().unwrap(), Some(mare_kernel::CursorRecord::Dropped { count: 1, .. })),
            "non-token cancel must drop without emitting degraded payload"
        );
        assert!(cursor.try_recv().unwrap().is_none());
        assert!(
            k.problems().iter().any(|p| p.contains("event tap for `request/prepare`")),
            "expected tap error for non-token cancel: {:?}",
            k.problems()
        );
    })
    .await;

    // 3. Real request path (`Host::prompt` -> `loop.lua` `emit_checked("request/prepare", { cancel = token })`)
    // must emit `request/prepare` on an ordinary tap without `kernel/error`.
    crate::common::local(async {
        let env = crate::common::TestEnv::new().await;
        let h = env.host().faux(serde_json::json!([{ "text": "ok" }])).no_session().build().await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let res = h.prompt("hello", serde_json::json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        let mut events = Vec::new();
        while let Ok(Some(rec)) = cursor.try_recv() {
            if let mare_kernel::CursorRecord::Event { event, .. } = rec {
                events.push(event);
            }
        }
        let prep = events.iter().find(|e| e.name == "request/prepare").expect("expected request/prepare event");
        assert!(prep.payload.get("cancel").is_none(), "cancel token must be projected out: {}", prep.payload);
        assert!(prep.payload["model"].is_object(), "expected model object in request/prepare: {}", prep.payload);
        assert!(!events.iter().any(|e| e.name == "kernel/error"), "unexpected kernel/error: {events:?}");
        assert!(h.kernel.problems().is_empty(), "unexpected kernel problems: {:?}", h.kernel.problems());
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn ordinary_tap_handles_large_history_above_10k_nodes() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let def = k
                .load_def(&src(
                    "req_probe",
                    r#"return {
                        name = "req_probe",
                        contracts = {
                            request = {
                                owner = "req_probe",
                                events = {
                                    build = { doc = "build", mode = "emit", expose = "lua" },
                                },
                            },
                        },
                        apply = function() end,
                    }"#,
                ))
                .unwrap();
            k.set_defs(vec![def]);
            k.start().await.unwrap();
            let mut cursor = k.open_cursor(std::sync::Arc::new(|_| Some(true)));

            // >10,000 JSON nodes (2,500 messages * 7 nodes = 17,500+ nodes) plus runtime-only fields.
            let large_req: mlua::Value = k
                .lua()
                .load(
                    r#"
                    local messages = {}
                    for i = 1, 2500 do
                        messages[i] = { role = "user", content = "msg-" .. i, idx = i }
                    end
                    return {
                        model = { provider = "faux", id = "echo" },
                        messages = messages,
                        cancel = mare.task.token(),
                        _rebuild_tool_declarations = function(r) return r end,
                    }
                    "#,
                )
                .eval()
                .unwrap();

            let mut args = mlua::MultiValue::new();
            args.push_back(large_req);
            k.emit_checked("request/build", args).await.unwrap();

            let Some(mare_kernel::CursorRecord::Event { event: ev, .. }) = cursor.try_recv().unwrap() else {
                panic!("expected large request/build tap event");
            };
            assert_eq!(ev.name, "request/build");
            assert_eq!(ev.payload["messages"].as_array().map(|a| a.len()), Some(2500));
            assert!(ev.payload.get("cancel").is_none());
            assert!(ev.payload.get("_rebuild_tool_declarations").is_none());
            assert!(k.problems().is_empty(), "unexpected tap problems: {:?}", k.problems());
        })
        .await;
}

/// An error in an event callback is isolated and must not disable the plugin for
/// subsequent events.
#[tokio::test]
async fn callback_error_does_not_disable_plugin() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let def = k
                .load_def(&src(
                    "flaky",
                    r#"return {
                        name = "flaky",
                        apply = function(ctx)
                            local count = 0
                            ctx:on("demo/ping", function(n)
                                count = count + 1
                                mare._flaky_seen = count
                                if n == 1 then error("boom once") end
                            end)
                        end,
                    }"#,
                ))
                .unwrap();
            k.set_defs(vec![def]);
            k.start().await.unwrap();
            k.emit("demo/ping", mlua::MultiValue::from_vec(vec![mlua::Value::Integer(1)])).await;
            k.emit("demo/ping", mlua::MultiValue::from_vec(vec![mlua::Value::Integer(2)])).await;
            let seen: i64 = k.lua().load("return mare._flaky_seen").eval().unwrap();
            assert_eq!(seen, 2, "plugin callback must keep firing after an earlier error");
            k.shutdown().await;
        })
        .await;
}
