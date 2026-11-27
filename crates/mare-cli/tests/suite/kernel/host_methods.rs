//! Host-provided contract methods (`provider = "host"`): dispatch through `mare.Service`
//! `guarded()`, rejection when a plugin tries to implement a host method, and peer authorization
//! on full operation names.

use std::rc::Rc;

use crate::common::*;
use mare_kernel::{CallerAccess, Kernel, Origin, PluginSource};
use mare_plugin::{self as lua, Env, Mode};
use mare_shared::ui::UiShared;
use serde_json::{Value, json};

fn host_method_kernel() -> Kernel {
    let l = lua::new_lua().unwrap();
    let env = Env {
        cwd: std::env::current_dir().unwrap(),
        mode: Mode::Child,
        ui: UiShared::new(),
        config: Default::default(),
        args: Value::Null,
    };
    lua::install(&l, &env).unwrap();
    let k = Kernel::new(l, Default::default()).unwrap();
    let contracts: mlua::Table = k
        .lua()
        .load(
            r#"return {
                mixed = {
                    methods = {
                        plugin_op = {
                            doc = "plugin method",
                            expose = "both",
                            params = { type = "object", properties = {}, additionalProperties = false },
                            order = {},
                            result = { type = "string" },
                        },
                        host_op = {
                            doc = "host method",
                            expose = "both",
                            provider = "host",
                            access = "peer",
                            params = { type = "object", properties = { x = { type = "integer" } }, required = { "x" }, additionalProperties = false },
                            order = { "x" },
                            result = { type = "integer" },
                        },
                    },
                },
            }"#,
        )
        .eval()
        .unwrap();
    let hooks_contract: mlua::Value =
        k.lua().load(lua::service_contracts_lua().as_ref()).eval::<mlua::Table>().unwrap().get("hooks").unwrap();
    contracts.set("hooks", hooks_contract).unwrap();
    k.install_service_contracts(contracts).unwrap();
    k
}

#[tokio::test]
async fn host_method_serves_lua_and_export_callers_through_one_guard() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        // 1. Built-in `kernel` exposes its new host-provided Lua methods through `guarded()`.
        let (has_list, has_caller, has_switch): (bool, bool, bool) = eval(
            &h,
            r#"
            local k = C.cordis:get("kernel")
            local ok_list, sessions = pcall(function() return k:list_sessions() end)
            local ok_caller = pcall(function() return k:get_caller() end)
            return ok_list and type(sessions) == "table", ok_caller, type(k.begin_session_switch) == "function"
            "#,
        )
        .await;
        assert!(has_list, "kernel:list_sessions must be available as a host-provided method");
        assert!(has_caller, "kernel:get_caller must be available as a host-provided method");
        assert!(has_switch, "kernel:begin_session_switch must be available as a host-provided method");

        // 2. `baseline` service contract was removed when `baseline.read` moved to mare-protocol:
        // direct kernel reads return `baseline.stale` on unknown handles.
        assert!(h.kernel.service_method("baseline", "read").is_none(), "baseline service contract must be removed");
        let caller = h.kernel.open_owner("host-method-test", "root").unwrap();
        let stale = h
            .kernel
            .read_baseline_page(caller, "missing-baseline", "entries", None, None)
            .expect_err("unknown baseline handle must return baseline.stale");
        assert_eq!(stale.code(), "baseline.stale");
        h.shutdown().await;
    })
    .await;
}

// guarantee: host-methods-stay-with-the-host
#[tokio::test]
async fn provider_cannot_implement_a_host_method() {
    local(async {
        let k = host_method_kernel();
        let bad_provider = r#"
        return {
            name = "bad-mixed",
            provide = { "mixed" },
            apply = function(ctx)
                local S = require("cordis").class("BadMixed", mare.Service)
                function S:constructor(c)
                    S.super.constructor(self, c, "mixed")
                end
                function S:plugin_op() return "from-plugin" end
                function S:host_op(x) return x + 1 end
                ctx:plugin(S)
            end,
        }
        "#;
        let def = k
            .load_def(&PluginSource {
                name_hint: "bad-mixed".into(),
                origin: Origin::User,
                path: None,
                code: Some(bad_provider.as_bytes()),
            })
            .unwrap();
        k.set_defs(vec![def]);
        k.start().await.unwrap();
        let problems = k.problems();
        assert!(
            problems.iter().any(|p| p.contains("invalid_arguments") || p.contains("host")),
            "provider implementing a provider = \"host\" method must fail admission: problems={problems:?}"
        );
        assert!(k.service("mixed").is_none(), "rejected provider must not publish mixed service");
        k.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn peer_without_grant_is_denied_a_host_method() {
    local(async {
        let k = host_method_kernel();
        let func = k.lua().create_function(|_, x: i64| Ok(x * 2)).unwrap();
        k.register_host_method("mixed", "host_op", func).unwrap();
        let provider = r#"
        return {
            name = "good-mixed",
            provide = { "mixed" },
            apply = function(ctx)
                local S = require("cordis").class("GoodMixed", mare.Service)
                function S:constructor(c)
                    S.super.constructor(self, c, "mixed")
                end
                function S:plugin_op() return "from-plugin" end
                ctx:plugin(S)
            end,
        }
        "#;
        let def = k
            .load_def(&PluginSource {
                name_hint: "good-mixed".into(),
                path: None,
                code: Some(provider.as_bytes()),
                origin: Origin::Builtin,
            })
            .unwrap();
        k.set_defs(vec![def]);
        k.start().await.unwrap();

        let host_spec = k.service_method("mixed", "host_op").expect("mixed.host_op export spec");
        assert!(host_spec.is_host(), "mixed.host_op must be a host-provided method");

        let denied_peer = k.open_owner("peer-denied", "root").unwrap();
        k.set_caller_access(
            denied_peer,
            Some(CallerAccess {
                is_peer: true,
                allows: Rc::new(|_svc, _method, _access| false),
                allows_session_path: Rc::new(|| false),
            }),
        );
        assert!(
            !k.permitted_for_caller(Some(denied_peer), "mixed", "host_op"),
            "peer without explicit grant must be denied host-provided mixed.host_op"
        );

        let allowed_peer = k.open_owner("peer-allowed", "root").unwrap();
        k.set_caller_access(
            allowed_peer,
            Some(CallerAccess {
                is_peer: true,
                allows: Rc::new(|svc, method, _access| svc == "mixed" && method == "host_op"),
                allows_session_path: Rc::new(|| false),
            }),
        );
        assert!(
            k.permitted_for_caller(Some(allowed_peer), "mixed", "host_op"),
            "peer with explicit full-name grant must be allowed host-provided mixed.host_op"
        );
        let res = k
            .call_service_export(
                allowed_peer,
                None,
                "mixed",
                "host_op",
                json!({ "x": 21 }),
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
        assert_eq!(res.expect("granted peer export call to host method must succeed"), json!(42));
        k.shutdown().await;
    })
    .await;
}

/// A different plugin running in a same-task hook (`policy:voter("session/before-switch")`)
/// before the outer service export reaches its commit boundary must NOT be able to mark the outer
/// export call committed via `kernel:mark_call_committed()`. Only the export provider's own owner
/// may mark its own export call committed.
#[tokio::test]
async fn foreign_plugin_mark_call_committed_cannot_bypass_outer_export_cancel() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("foreign-committer.lua"),
            r#"return {
                name = "foreign-committer",
                inject = { "policy", "kernel" },
                apply = function(ctx)
                    ctx.policy:voter("session/before-switch", function()
                        if mare._foreign_try_commit then
                            ctx.kernel:mark_call_committed()
                            if mare._foreign_notified_tx then
                                mare._foreign_notified_tx:send(true)
                            end
                            if mare._foreign_hold_rx then
                                mare._foreign_hold_rx:recv()
                            end
                        end
                    end)
                end,
            }"#,
        )
        .unwrap();

        let h = env.host().user_plugins().faux(json!([{ "text": "ok" }])).build().await;
        exec(
            &h,
            r#"
            mare._foreign_try_commit = true
            mare._foreign_notified_tx, mare._foreign_notified_rx = mare.task.channel()
            mare._foreign_hold_tx, mare._foreign_hold_rx = mare.task.channel()
            "#,
        )
        .await;

        let caller = h.kernel.open_owner("rpc-cancel-test", "root").unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let h_call = h.clone();
        let cancel_call = cancel.clone();
        let task = tokio::task::spawn_local(async move {
            h_call.kernel.call_service_export(caller, None, "session", "create", json!({}), cancel_call).await
        });

        // Wait for foreign-committer's pre-commit voter to invoke mark_call_committed().
        let reached: bool =
            eval(&h, "return mare.task.timeout(5000, function() return mare._foreign_notified_rx:recv() end)").await;
        assert!(reached, "foreign voter must have run before cancel");

        // Cancel the RPC call while paused in the foreign pre-commit hook.
        cancel.cancel();
        let res = tokio::time::timeout(std::time::Duration::from_secs(3), task)
            .await
            .expect("cancelled export must not hang waiting on foreign hook")
            .unwrap();
        assert!(
            matches!(res, Err(mare_kernel::ServiceCallError::Cancelled)),
            "foreign plugin's mark_call_committed must NOT suppress outer export cancellation: {res:?}"
        );

        exec(&h, "mare._foreign_try_commit = false; mare._foreign_hold_tx:send(true)").await;
        h.shutdown().await;
    })
    .await;
}

/// `CALL_COMMITTED` is task-local; when the same task enters a second host's kernel whose plugin
/// has the exact same numeric owner id, calling `kernel:mark_call_committed()` on the second
/// kernel must NOT mark the first kernel's outer export call as committed.
#[tokio::test]
async fn cross_kernel_mark_call_committed_does_not_suppress_outer_export_cancel() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();

        let svc_plugin = r#"return {
            name = "cross-svc",
            inject = { "kernel" },
            provide = { "cross-svc" },
            contracts = {
                ["cross-svc"] = {
                    methods = {
                        ping = {
                            doc = "gated export",
                            expose = "both",
                            params = { type = "object", properties = {}, additionalProperties = false },
                            order = {},
                            result = { type = "boolean" },
                        },
                        mark = {
                            doc = "call kernel:mark_call_committed as cross-svc owner",
                            expose = "lua",
                            params = { type = "object", properties = {}, additionalProperties = false },
                            order = {},
                            result = { type = "boolean" },
                        },
                    },
                },
            },
            apply = function(ctx)
                local Svc = require("cordis").class("CrossSvc", mare.Service)
                function Svc:constructor(c)
                    Svc.super.constructor(self, c, "cross-svc")
                end
                function Svc:ping()
                    if mare._cross_bridge then
                        mare._cross_bridge()
                    end
                    return true
                end
                function Svc:mark()
                    ctx.kernel:mark_call_committed()
                    return true
                end
                ctx:plugin(Svc)
            end,
        }"#;
        std::fs::write(pdir.join("cross-svc.lua"), svc_plugin).unwrap();

        let h_a = env.host().user_plugins().faux(json!([])).build().await;
        let h_b = env.host().user_plugins().faux(json!([])).build().await;

        let owner_a = h_a.kernel.service_provider("cross-svc");
        let owner_b = h_b.kernel.service_provider("cross-svc");
        assert!(owner_a.is_some() && owner_b.is_some(), "both hosts must provide cross-svc");

        // Bridge from h_a's `cross-svc.ping` into h_b's `cross-svc:mark()` on the SAME tokio task
        // (same numeric plugin owner id, different Kernel!), then wait on a gate so we can cancel h_a.
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let hold = std::sync::Arc::new(tokio::sync::Notify::new());
        let entered_cb = std::sync::Arc::clone(&entered);
        let hold_cb = std::sync::Arc::clone(&hold);
        let h_b_cb = h_b.clone();
        let bridge = h_a
            .lua()
            .create_async_function(move |_, ()| {
                let entered_cb = std::sync::Arc::clone(&entered_cb);
                let hold_cb = std::sync::Arc::clone(&hold_cb);
                let h_b_cb = h_b_cb.clone();
                async move {
                    let svc_b = h_b_cb.service_table("cross-svc").unwrap();
                    let mark_fn: mlua::Function = svc_b.get("mark")?;
                    let _: bool = mark_fn.call_async(svc_b).await?;
                    entered_cb.notify_one();
                    hold_cb.notified().await;
                    Ok(())
                }
            })
            .unwrap();
        let mare_tbl: mlua::Table = h_a.lua().globals().get("mare").unwrap();
        mare_tbl.set("_cross_bridge", bridge).unwrap();

        let caller_a = h_a.kernel.open_owner("cross-caller", "root").unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let h_a_call = h_a.clone();
        let cancel_call = cancel.clone();
        let task = tokio::task::spawn_local(async move {
            h_a_call.kernel.call_service_export(caller_a, None, "cross-svc", "ping", json!({}), cancel_call).await
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .expect("h_a export must invoke cross-kernel bridge into h_b");
        cancel.cancel();

        let waited = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
        hold.notify_waiters();
        let res = waited.expect("cancelled h_a export must not hang when h_b called mark_call_committed").unwrap();
        assert!(
            matches!(res, Err(mare_kernel::ServiceCallError::Cancelled)),
            "h_b's kernel:mark_call_committed must NOT suppress h_a's outer export cancellation: {res:?}"
        );

        h_a.shutdown().await;
        h_b.shutdown().await;
    })
    .await;
}

/// Async host methods invoked with `consumer_owner == nil` must strip any outer
/// plugin OWNER; (b) a disposed plugin fiber / closed owner must be refused `request_lease` and
/// `begin_session_switch` rather than silently obtaining an unowned lock; (c) a
/// `SessionSwitchHandle` whose originating coroutine exits without `<close>` must auto-abort.
#[tokio::test]
async fn host_async_strips_owner_dead_fiber_refused_and_switch_aborts_on_coro_exit() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("stale-holder.lua"),
            r#"return {
                name = "stale-holder",
                inject = { "bus", "kernel" },
                contracts = {
                    test = {
                        owner = "stale-holder",
                        events = {
                            ["start-root-switch"] = { doc = "test", mode = "emit", expose = "process" },
                        },
                    },
                },
                apply = function(ctx)
                    mare._saved_plugin_kernel = ctx.kernel
                    local root_ctx = ctx[require("cordis").Context.root]
                    ctx:on("test/start-root-switch", function()
                        -- Runs inside `stale-holder`'s plugin OWNER scope, but calls
                        -- `begin_session_switch` via root ctx (`consumer_owner == nil`).
                        local sw <close> = root_ctx:get("kernel"):begin_session_switch({ session_id = "root-owned-sid" })
                        mare._root_sw_ready_tx:send(true)
                        mare._root_sw_commit_rx:recv()
                        local ok, res = pcall(function()
                            return sw:commit("root-owned-sid").session_epoch
                        end)
                        mare._root_sw_done_tx:send(ok and tostring(res) or ("ERR:" .. tostring(res)))
                    end)
                end,
            }"#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);

        // (a) Async host method called with root ctx (`consumer_owner == nil`) from INSIDE
        // `stale-holder`'s plugin owner scope must strip outer OWNER: disposing `stale-holder`
        // must NOT cancel the root-owned switch handle.
        exec(
            &h,
            r#"
            mare._root_sw_ready_tx, mare._root_sw_ready_rx = mare.task.channel()
            mare._root_sw_commit_tx, mare._root_sw_commit_rx = mare.task.channel()
            mare._root_sw_done_tx, mare._root_sw_done_rx = mare.task.channel()
            mare.task.spawn(function()
                C.cordis:get("bus"):emit("test/start-root-switch")
            end)
            assert(mare.task.timeout(5000, function() return mare._root_sw_ready_rx:recv() end))
            "#,
        )
        .await;

        // (b) Dispose `stale-holder` completely (remove its plugin file and run `reload_all`) so
        // its old `ctx.fiber` stays DISPOSED and its owner is closed.
        let stale_path = pdir.join("stale-holder.lua");
        std::fs::remove_file(&stale_path).unwrap();
        h.reload_all().await.unwrap();

        // The root-owned switch started inside `stale-holder`'s scope must SURVIVE `stale-holder`'s
        // disposal and commit cleanly (proving outer plugin OWNER was stripped).
        let root_commit: String = eval(
            &h,
            r#"
            mare._root_sw_commit_tx:send(true)
            local ok, val = mare.task.timeout(5000, function() return mare._root_sw_done_rx:recv() end)
            return ok and tostring(val) or "TIMEOUT"
            "#,
        )
        .await;
        assert!(
            root_commit.starts_with("root-owned-sid#"),
            "root-owned switch started inside plugin scope must survive plugin disposal: {root_commit}"
        );

        let (lease_ok, lease_err, switch_ok, switch_err): (bool, String, bool, String) = eval(
            &h,
            r#"
            local k = mare._saved_plugin_kernel
            local ok_l, lease_or_err, reason = pcall(function() return k:request_lease() end)
            local got_lease = ok_l and lease_or_err ~= nil
            if got_lease then lease_or_err:release() end
            local ok_s, sw_or_err = pcall(function() return k:begin_session_switch({ session_id = "dead-fiber-sid" }) end)
            if ok_s and sw_or_err then sw_or_err:abort() end
            return got_lease, tostring(ok_l and reason or lease_or_err), ok_s, tostring(sw_or_err)
            "#,
        )
        .await;
        assert!(!lease_ok, "disposed fiber must NOT acquire request_lease: {lease_err}");
        assert!(!switch_ok, "disposed fiber must NOT acquire begin_session_switch: {switch_err}");

        // (c) A `SessionSwitchHandle` acquired in a coroutine without `<close>` and retained in a
        // global (preventing GC) must auto-abort once that coroutine finishes or errors.
        let (reclaimed_after_return, reclaimed_after_error): (bool, bool) = eval(
            &h,
            r#"
            local k = C.cordis:get("kernel")
            local done1_tx, done1_rx = mare.task.channel()
            mare.task.spawn(function()
                mare._leaked_sw_1 = k:begin_session_switch({ session_id = "coro-return-sid" })
                done1_tx:send(true)
            end)
            assert(mare.task.timeout(5000, function() return done1_rx:recv() end))
            mare.task.sleep(10)
            local ok1, sw1 = pcall(function() return k:begin_session_switch({ session_id = "after-return-sid" }) end)
            if ok1 and sw1 then sw1:abort() end

            local done2_tx, done2_rx = mare.task.channel()
            mare.task.spawn(function()
                local ok = pcall(function()
                    mare._leaked_sw_2 = k:begin_session_switch({ session_id = "coro-err-sid" })
                    done2_tx:send(true)
                end)
                if ok then error("boom") end
            end)
            assert(mare.task.timeout(5000, function() return done2_rx:recv() end))
            mare.task.sleep(10)
            local ok2, sw2 = pcall(function() return k:begin_session_switch({ session_id = "after-err-sid" }) end)
            if ok2 and sw2 then sw2:abort() end
            return ok1, ok2
            "#,
        )
        .await;
        assert!(reclaimed_after_return, "switch leaked by finished coroutine must auto-abort");
        assert!(reclaimed_after_error, "switch leaked by errored coroutine must auto-abort");

        h.shutdown().await;
    })
    .await;
}

/// Review #3a: Verify an async `provider = "host"` method invoked with `consumer_owner == nil`
/// inside an outer plugin's owner scope strips outer `OWNER` (`active_owner_id() == None`).
#[tokio::test]
async fn async_host_method_strips_outer_plugin_owner_when_consumer_is_root() {
    local(async {
        let k = host_method_kernel();
        k.register_host_method_async(
            "mixed",
            "host_op",
            k.lua()
                .create_async_function(|_, _x: i64| async move {
                    Ok(mare_kernel::active_owner_id().map(|id| id as i64).unwrap_or(-1))
                })
                .unwrap(),
        )
        .unwrap();

        let def_provider = k
            .load_def(&PluginSource {
                name_hint: "good-mixed".into(),
                origin: Origin::User,
                path: None,
                code: Some(
                    br#"return {
                        name = "good-mixed",
                        provide = { "mixed" },
                        apply = function(ctx)
                            local S = require("cordis").class("GoodMixed", mare.Service)
                            function S:constructor(c)
                                S.super.constructor(self, c, "mixed")
                            end
                            function S:plugin_op() return "ok" end
                            ctx:plugin(S)
                        end,
                    }"#,
                ),
            })
            .unwrap();
        let def_outer = k
            .load_def(&PluginSource {
                name_hint: "outer-caller".into(),
                origin: Origin::User,
                path: None,
                code: Some(
                    br#"return {
                        name = "outer-caller",
                        inject = { "mixed" },
                        apply = function(ctx)
                            mare._invoke_from_outer = function(root_mixed)
                                return root_mixed:host_op(1)
                            end
                            -- Wrap via ctx:on so calling bus:emit runs inside outer-caller's OWNER,
                            -- while invoking `mixed:host_op` through the root Cordis context (`consumer_owner == nil`).
                            local root_ctx = ctx[require("cordis").Context.root]
                            ctx:on("test/probe-owner", function()
                                local root_mixed = root_ctx:get("mixed")
                                mare._observed_owner = root_mixed:host_op(1)
                            end)
                        end,
                    }"#,
                ),
            })
            .unwrap();
        k.set_defs(vec![def_provider, def_outer]);
        k.start().await.unwrap();

        k.emit("test/probe-owner", mlua::MultiValue::new()).await;
        let observed: i64 = k.lua().globals().get::<mlua::Table>("mare").unwrap().get("_observed_owner").unwrap();
        assert_eq!(observed, -1, "async host method via root ctx must strip outer plugin OWNER (got owner {observed})");
        k.shutdown().await;
    })
    .await;
}

/// `Host::bind_session_call` and `kernel:begin_session_switch` / `:commit` are
/// mutually exclusive without TOCTOU races.
#[tokio::test]
async fn bind_session_call_and_begin_switch_are_mutually_exclusive() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);

        // 1. Hold an active switch inside a live coroutine: `bind_session_call()` must be refused.
        exec(
            &h,
            r#"
            local k = C.cordis:get("kernel")
            local ready_tx, ready_rx = mare.task.channel()
            mare._race_rel_tx, mare._race_rel_rx = mare.task.channel()
            mare.task.spawn(function()
                local sw <close> = k:begin_session_switch({ session_id = "race-1" })
                ready_tx:send(true)
                mare._race_rel_rx:recv()
                sw:abort()
            end)
            assert(mare.task.timeout(5000, function() return ready_rx:recv() end))
            "#,
        )
        .await;
        assert!(h.bind_session_call().is_err(), "bind_session_call must fail while a session switch is in progress");

        // 2. Releasing/aborting the switch allows `bind_session_call()`.
        exec(&h, "mare._race_rel_tx:send(true); mare.task.sleep(10)").await;
        let guard = h.bind_session_call().expect("bind_session_call must succeed after switch abort");

        // 3. While `SessionCallGuard` is held, `begin_session_switch` fails with `busy`.
        let (ok_sw, err_sw): (bool, String) = eval(
            &h,
            r#"
            local ok, err = pcall(function()
                return C.cordis:get("kernel"):begin_session_switch({ session_id = "race-2" })
            end)
            return ok, tostring(type(err) == "table" and (err.code or err.message) or err)
            "#,
        )
        .await;
        assert!(!ok_sw && err_sw.contains("busy"), "expected busy while guard held: ok={ok_sw}, err={err_sw}");

        // 4. Dropping `guard` lets switch + commit succeed.
        drop(guard);
        let epoch: String = eval(
            &h,
            r#"
            local sw <close> = C.cordis:get("kernel"):begin_session_switch({ session_id = "race-2" })
            return sw:commit("race-2").session_epoch
            "#,
        )
        .await;
        assert!(!epoch.is_empty());

        h.shutdown().await;
    })
    .await;
}
