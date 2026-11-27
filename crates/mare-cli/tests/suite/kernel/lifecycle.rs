use super::{CONSUMER, Origin, PluginSource, REGISTRY, kernel, src};

// guarantee: ctx-registrations-withdrawn-on-dispose
#[tokio::test]
async fn dispose_reclaims_everything() {
    // Lua is !Send: everything touching the VM runs on a LocalSet (actor model)
    tokio::task::LocalSet::new().run_until(dispose_reclaims_everything_inner()).await
}

async fn dispose_reclaims_everything_inner() {
    let k = kernel();
    let defs = vec![k.load_def(&src("registry", REGISTRY)).unwrap(), k.load_def(&src("consumer", CONSUMER)).unwrap()];
    k.set_defs(defs);
    k.start().await.unwrap();
    assert!(
        crate::common::plugin_active(&k, "consumer"),
        "status {:?} tree {:?} problems {:?}",
        k.plugin_status(),
        k.plugin_tree().map(|t| t.iter().map(|r| format!("{r:?}")).collect::<Vec<_>>()),
        k.problems()
    );
    let lua = k.lua().clone();
    // the provider as it is now: a reference kept across a restart of its fiber is refused
    // (`unavailable`), so each count reads the current one
    let count = || -> i64 {
        lua.globals().set("__reg", k.service("test-registry").unwrap()).unwrap();
        lua.load("return _G.__reg:count()").eval().unwrap()
    };
    assert_eq!(count(), 2);

    // events: emit / bail / waterfall
    k.emit("ping", mlua::MultiValue::from_vec(vec![mlua::Value::Integer(3)])).await;
    let m: mlua::Table = lua.globals().get("mare").unwrap();
    assert_eq!(m.get::<i64>("_pings").unwrap(), 3);
    assert!(lua.globals().get::<Option<bool>>("leak").unwrap().is_none(), "plugin globals are isolated");
    assert!(!m.get::<bool>("_std_patch").unwrap(), "standard library is read-only");
    let v = k.bail("ask", mlua::MultiValue::new()).await;
    assert_eq!(v.as_string().unwrap().to_str().unwrap(), "consumer");
    let v = k.waterfall("rewrite", mlua::Value::String(lua.create_string("x").unwrap()), mlua::MultiValue::new()).await;
    assert_eq!(v.as_string().unwrap().to_str().unwrap(), "x+c");

    // the consumer's three listeners, counted on the bus per event
    let consumer_events = ["ping", "ask", "rewrite"];
    let listeners = || consumer_events.iter().map(|e| k.listener_count(e)).sum::<usize>();
    let listeners_before = listeners();
    assert!(listeners_before >= 3, "{listeners_before}");
    // reclamation is checked directly below: items, listeners, the cleanup
    crate::common::unload_plugin(&k, "consumer").await;
    assert!(!crate::common::plugin_active(&k, "consumer"));
    assert_eq!(count(), 0, "items registered through ctx are removed");
    assert_eq!(listeners(), listeners_before - 3, "listeners removed");
    assert_eq!(m.get::<i64>("_disposed").unwrap(), 1);
    assert_eq!(k.bail("ask", mlua::MultiValue::new()).await, mlua::Value::Nil);

    // the consumer comes back (its definition again, a reload): its registrations come back with it.
    // A provider reload restarting its dependents is ports
    // a_replaced_provider_disposes_its_dependents_first_and_restarts_them_on_the_new_one; whether
    // they are listed in the reload's `applied` is open (the reload checklist).
    let defs = vec![k.load_def(&src("registry", REGISTRY)).unwrap(), k.load_def(&src("consumer", CONSUMER)).unwrap()];
    k.sync_defs(defs).await;
    k.reload(None).await.unwrap();
    assert!(crate::common::plugin_active(&k, "consumer"), "{:?}", k.problems());
    assert_eq!(count(), 2);
    k.shutdown().await;
    assert_eq!(listeners(), 0);
}

// The provider-stack tests (shadowing_and_cycles, user_override_beats_builtin_regardless_of_apply_order,
// cycles_only_fail_their_members, held_service_reference_falls_back_after_provider_dispose) went
// with the stack (the stack-based implementation). Their single-slot replacements are in
// semantics.rs: a_second_provider_of_a_name_is_refused_naming_both_files,
// a_taken_slot_refuses_a_second_provide_and_nothing_falls_back,
// a_plugin_cannot_take_a_builtins_service_without_turning_it_off,
// a_dependency_cycle_waits_and_the_rest_starts; `override = true` stays in plugins/overrides.rs.

fn file_src(dir: &std::path::Path, name: &str, code: &str) -> PluginSource {
    let p = dir.join(format!("{name}.lua"));
    std::fs::write(&p, code).unwrap();
    PluginSource { name_hint: name.into(), origin: Origin::User, path: Some(p), code: None }
}

fn write_bumped(path: &std::path::Path, code: &str, secs: u64) {
    std::fs::write(path, code).unwrap();
    let t = std::time::SystemTime::now() + std::time::Duration::from_secs(secs);
    std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
}

// guarantee: failed-reload-keeps-the-old-definition-serving
#[tokio::test]
async fn a_full_reload_keeps_a_plugin_whose_new_definition_cannot_start() {
    tokio::task::LocalSet::new().run_until(reload_keeps_old_inner()).await
}

async fn reload_keeps_old_inner() {
    let k = kernel();
    let d = tempfile::tempdir().unwrap();
    let a = file_src(
        d.path(),
        "a",
        r#"return { name = "a", provide = {"x"}, apply = function(ctx) mare._a = "v1"; local S = require("cordis").class("S", mare.Service); function S:constructor(c) S.super.constructor(self, c, "x") end; ctx:plugin(S) end }"#,
    );
    let b = file_src(
        d.path(),
        "b",
        r#"return { name = "b", provide = {"y"}, inject = {"x"}, apply = function(ctx) local S = require("cordis").class("S", mare.Service); function S:constructor(c) S.super.constructor(self, c, "y") end; ctx:plugin(S) end }"#,
    );
    k.set_defs(vec![k.load_def(&a).unwrap(), k.load_def(&b).unwrap()]);
    k.start().await.unwrap();
    // a's new definition closes a cycle with b
    std::fs::write(
        a.path.as_ref().unwrap(),
        r#"return { name = "a", provide = {"x"}, inject = {"y"}, apply = function(ctx) mare._a = "v2"; local S = require("cordis").class("S", mare.Service); function S:constructor(c) S.super.constructor(self, c, "x") end; ctx:plugin(S) end }"#,
    )
    .unwrap();
    k.reload(None).await.unwrap();
    assert!(
        crate::common::plugin_active(&k, "a") && crate::common::plugin_active(&k, "b"),
        "neither is dropped: {:?}",
        k.problems()
    );
    let v: String = k.lua().load("return mare._a").eval().unwrap();
    assert_eq!(v, "v1", "a runs its previous definition");
    // the new definition is refused before the running one is stopped (the cycle is a static
    // refusal of the host's row list, cordis-core #8), so the running one is kept — not
    // "re-enabled", which is for a new definition that failed after the old one stopped; and a
    // cycle is not "(pending)" by design
    let problems = k.problems();
    assert!(problems.iter().any(|p| p.contains("plugin a") && p.contains("keeping the running one")), "{problems:?}");
    assert!(!problems.iter().any(|p| p.contains("(pending)")), "a cycle is refused, not pending: {problems:?}");
}

/// stage 2: a plugin file's top level may only `return { ... }`. Reading `mare` /
/// `require` / io there, or setting a global, is refused when the definition loads; pure
/// standard-library helpers for constant tables are fine, and `apply` sees the normal env.
#[tokio::test]
async fn plugin_top_level_may_only_return_its_table() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            for (name, code, what) in [
                ("reads-mare", "local fs = mare.fs\nreturn { name = 'reads-mare' }", "reads `mare`"),
                ("requires", "local x = require('reqs.x')\nreturn {}", "reads `require`"),
                ("prints", "print('hi')\nreturn {}", "reads `print`"),
                ("sets-global", "helper = 1\nreturn {}", "sets `helper`"),
                ("via-g", "_G.helper = 1\nreturn {}", "sets `helper`"),
            ] {
                let err = k.load_def(&src(name, code)).err().unwrap_or_else(|| panic!("{name} loaded")).to_string();
                assert!(err.contains("top level of a plugin file may only") && err.contains(what), "{name}: {err}");
                assert!(err.contains(name), "the error names the plugin: {err}");
                assert!(err.contains(&format!("builtin/{name}.lua")), "the error names the file: {err}");
            }
            for (name, code, what) in [
                ("returns-nothing", "local t = {}", "returned nil"),
                ("returns-string", "return 'x'", "returned string"),
            ] {
                let err = k.load_def(&src(name, code)).err().unwrap_or_else(|| panic!("{name} loaded")).to_string();
                assert!(err.contains("must end in `return { ... }`") && err.contains(what), "{name}: {err}");
                assert!(err.contains(&format!("builtin/{name}.lua")), "the error names the file: {err}");
            }
            let def = k
                .load_def(&src(
                    "pure",
                    r#"local LEVELS = {}
                    for i, n in ipairs({ "low", "high" }) do LEVELS[n] = i end
                    local FMT = string.format("%s-%d", "x", math.max(1, 2))
                    return { name = "pure", levels = LEVELS, fmt = FMT, apply = function() return mare ~= nil end }"#,
                ))
                .unwrap();
            assert_eq!(def.table.get::<String>("fmt").unwrap(), "x-2");
            assert!(def.env.get::<mlua::Value>("mare").unwrap().is_table(), "the normal env is back after loading");
            let apply: mlua::Function = def.table.get("apply").unwrap();
            assert!(apply.call::<bool>(()).unwrap(), "apply reads mare");
            def.env.set("later", 1).unwrap();
            assert_eq!(def.env.get::<i64>("later").unwrap(), 1, "globals can be set after loading");
        })
        .await
}

#[tokio::test]
async fn shutdown_drains_spawned_coroutine_close_before_releasing_its_scope() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    tokio::task::LocalSet::new()
        .run_until(async {
          for inherited in [false, true] {
            let owner = tempfile::tempdir().unwrap();
            let directory = owner.path().join("fixture");
            std::fs::create_dir(&directory).unwrap();
            let marker = directory.join("closed-scope");
            let closed = Arc::new(AtomicBool::new(false));
            let k = kernel();
            let mare: mlua::Table = k.lua().globals().get("mare").unwrap();
            let (entered, entry) = tokio::sync::oneshot::channel();
            let entered = std::cell::RefCell::new(Some(entered));
            mare.set(
                "_entered",
                k.lua()
                    .create_function(move |_, ()| {
                        entered.borrow_mut().take().unwrap().send(()).unwrap();
                        Ok(())
                    })
                    .unwrap(),
            )
            .unwrap();
            let observed = closed.clone();
            let restored = marker.clone();
            mare.set(
                "_closed",
                k.lua()
                    .create_function(move |lua, ()| {
                        let current = mare_shared::exec_ctx::current(lua);
                        assert_eq!(current.is_some(), inherited, "close must retain its execution context");
                        if let Some(current) = current {
                            assert_eq!(current.get::<String>("scope_marker")?, "owned-close-scope");
                        }
                        std::fs::create_dir_all(&restored).map_err(mlua::Error::external)?;
                        observed.store(true, Ordering::Release);
                        Ok(())
                    })
                    .unwrap(),
            )
            .unwrap();
            mare.set(
                "_park",
                k.lua()
                    .create_async_function(|_, ()| async { std::future::pending::<mlua::Result<()>>().await })
                    .unwrap(),
            )
            .unwrap();
            let def = k
                .load_def(&src(
                    "close-scope",
                    r#"return {
            name = "close-scope", inject = { "task" }, apply = function(ctx)
                mare._launch = function()
                    ctx:spawn(function()
                        local scope <close> = setmetatable({}, {__close = function() mare._closed() end})
                        mare._entered()
                        mare._park()
                    end)
                end
            end,
        }"#,
                ))
                .unwrap();
            k.set_defs(vec![def]);
            k.start().await.unwrap();
            let launch: mlua::Function = mare.get("_launch").unwrap();
            if inherited {
                let context = k.lua().create_table().unwrap();
                context.set("scope_marker", "owned-close-scope").unwrap();
                mare_shared::exec_ctx::run_in(k.lua(), context, launch, mlua::MultiValue::new()).await.unwrap();
            } else {
                launch.call::<()>(()).unwrap();
            }
            entry.await.unwrap();
            k.shutdown().await;
            let closed_at_shutdown = closed.load(Ordering::Acquire);
            std::fs::remove_dir_all(&directory).unwrap();
            tokio::task::yield_now().await;
            let recreated_after_shutdown = marker.exists();
            if directory.exists() {
                std::fs::remove_dir_all(&directory).unwrap();
            }
            assert!(
                closed_at_shutdown,
                "shutdown returned before coroutine __close (inherited={inherited}); recreated scope after removal: {recreated_after_shutdown}"
            );
            assert!(!recreated_after_shutdown, "cancelled coroutine recreated its released scope");
            owner.close().unwrap();
          }
        })
        .await;
}

/// When a new definition fails loading, compilation, or pre-apply validation
/// (syntax error, top-level guard, self-rename, non-function `apply`, dependency cycle), the old
/// instance is untouched (never disposed or re-applied), its in-memory state is preserved, and a
/// service call returns the exact same result — both for targeted reload and full reload.
// guarantee: broken-definition-reload-changes-nothing
#[tokio::test]
async fn reload_pre_apply_validation_failure_leaves_old_instance_untouched() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let dir = tempfile::tempdir().unwrap();
            let v1 = r#"return {
                name = "prov",
                provide = { "svc" },
                apply = function(ctx)
                    mare._prov_applies = (mare._prov_applies or 0) + 1
                    local calls = 0
                    local Svc = require("cordis").class("Svc", mare.Service)
                    function Svc:constructor(c) Svc.super.constructor(self, c, "svc") end
                    function Svc:call()
                        calls = calls + 1
                        return "v1:" .. calls
                    end
                    ctx:plugin(Svc)
                    ctx:effect(function() return function()
                        mare._prov_disposes = (mare._prov_disposes or 0) + 1
                    end end)
                end,
            }"#;
            let dep_code = r#"return {
                name = "dep",
                inject = { "svc" },
                provide = { "x" },
                apply = function(ctx)
                    mare._dep_applies = (mare._dep_applies or 0) + 1
                    local X = require("cordis").class("X", mare.Service)
                    function X:constructor(c) X.super.constructor(self, c, "x") end
                    ctx:plugin(X)
                    ctx:effect(function() return function()
                        mare._dep_disposes = (mare._dep_disposes or 0) + 1
                    end end)
                end,
            }"#;
            let prov = file_src(dir.path(), "prov", v1);
            let dep = file_src(dir.path(), "dep", dep_code);
            k.set_defs(vec![k.load_def(&prov).unwrap(), k.load_def(&dep).unwrap()]);
            k.start().await.unwrap();
            assert!(crate::common::plugin_active(&k, "prov") && crate::common::plugin_active(&k, "dep"));

            let lua = k.lua().clone();
            let call_svc = || -> String {
                let svc = k.service("svc").unwrap();
                lua.globals().set("__m7_4_svc", svc).unwrap();
                lua.load("return __m7_4_svc.call()").eval().unwrap()
            };
            assert_eq!(call_svc(), "v1:1");

            let invalid_defs = [
                ("syntax", "return { name = 'prov', apply = function( end }"),
                ("top-level-guard", "local _ = mare.fs\nreturn { name = 'prov' }"),
                ("rename", r#"return { name = "renamed", provide = { "svc" } }"#),
                ("non-fn-apply", r#"return { name = "prov", provide = { "svc" }, apply = 123 }"#),
                (
                    "cycle",
                    r#"return { name = "prov", provide = { "svc" }, inject = { "x" } }"#,
                ),
            ];

            let mut expected_calls = 1;
            let mut expected_dep_applies = 1;
            let mut expected_dep_disposes = 0;
            for (idx, (kind, bad_code)) in invalid_defs.into_iter().enumerate() {
                write_bumped(prov.path.as_ref().unwrap(), bad_code, (idx as u64 + 1) * 2);
                let err = k
                    .reload(Some("prov"))
                    .await
                    .expect_err(&format!("{kind}: targeted reload must fail"));
                assert!(
                    err.to_string().contains("aborted, nothing changed"),
                    "{kind}: unexpected error: {err}"
                );

                // Old instance and its dependent were never disposed or re-applied.
                let counts: (i64, i64, i64, i64) = lua
                    .load("return mare._prov_applies, mare._prov_disposes or 0, mare._dep_applies, mare._dep_disposes or 0")
                    .eval()
                    .unwrap();
                assert_eq!(
                    counts,
                    (1, 0, expected_dep_applies, expected_dep_disposes),
                    "{kind}: old instances must remain untouched"
                );

                // Service call still hits the exact same old instance with preserved state.
                expected_calls += 1;
                assert_eq!(call_svc(), format!("v1:{expected_calls}"), "{kind}: service state preserved");

                // Full reload also keeps the old `prov` instance untouched and reports the failure.
                let report = k.reload_report(None).await.unwrap();
                assert!(!report.failures.is_empty(), "{kind}: full reload must report failure");
                assert!(!report.applied.contains(&"prov".to_string()), "{kind}: must not re-apply prov");
                // A user's full reload restarts every plugin it prepared, as on the old path (a
                // built-in reading files in `apply` picks up edits); `prov` failed to prepare and
                // keeps its old instance, `dep` restarts once.
                expected_dep_applies += 1;
                expected_dep_disposes += 1;
                let counts_after_full: (i64, i64, i64, i64) = lua
                    .load("return mare._prov_applies, mare._prov_disposes or 0, mare._dep_applies, mare._dep_disposes or 0")
                    .eval()
                    .unwrap();
                assert_eq!(
                    counts_after_full,
                    (1, 0, expected_dep_applies, expected_dep_disposes),
                    "{kind}: full reload must not dispose/re-apply prov"
                );
                expected_calls += 1;
                assert_eq!(call_svc(), format!("v1:{expected_calls}"), "{kind}: service state preserved after full reload");
            }

            k.shutdown().await;
        })
        .await;
}

/// When switching to a new definition whose `apply` fails, the old instance is
/// disposed *before* the new `apply` runs (the new `apply` observes the old instance as stopped),
/// ctx-held resources registered by the failed `apply` are cleaned up, external side effects from
/// the failed `apply` are not rolled back, the old definition is re-enabled as a fresh instance
/// (its old instance state is not restored), dependents are re-applied, and the failure is reported.
// guarantee: failed-reload-keeps-the-old-definition-serving
#[tokio::test]
async fn reload_apply_failure_reenables_old_def_as_fresh_instance_and_reports_error() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let dir = tempfile::tempdir().unwrap();
            let v1 = r#"return {
                name = "prov",
                provide = { "svc" },
                apply = function(ctx)
                    mare._events = mare._events or {}
                    mare._prov_applies = (mare._prov_applies or 0) + 1
                    local n = mare._prov_applies
                    mare._old_stopped = false
                    table.insert(mare._events, "old_apply:" .. n)
                    local calls = 0
                    local Svc = require("cordis").class("Svc", mare.Service)
                    function Svc:constructor(c)
                        Svc.super.constructor(self, c, "svc")
                        -- in the service's own fiber (ctx:plugin runs it as a child): the dependents
                        -- follow that fiber and go before this runs
                        c:effect(function() return function()
                            mare._prov_disposes = (mare._prov_disposes or 0) + 1
                            mare._old_stopped = true
                            table.insert(mare._events, "old_dispose:" .. n)
                        end end)
                    end
                    function Svc:call()
                        calls = calls + 1
                        return "v1:" .. calls
                    end
                    ctx:plugin(Svc)
                    ctx:on("probe/ping", function()
                        mare._old_pings = (mare._old_pings or 0) + 1
                    end)
                end,
            }"#;
            let dep_code = r#"return {
                name = "dep",
                inject = { "svc" },
                apply = function(ctx)
                    mare._events = mare._events or {}
                    mare._dep_applies = (mare._dep_applies or 0) + 1
                    local n = mare._dep_applies
                    table.insert(mare._events, "dep_apply:" .. n)
                    mare._dep_saw = ctx.svc:call()
                    ctx:effect(function() return function()
                        mare._dep_disposes = (mare._dep_disposes or 0) + 1
                        table.insert(mare._events, "dep_dispose:" .. n)
                    end end)
                end,
            }"#;
            let prov = file_src(dir.path(), "prov", v1);
            let dep = file_src(dir.path(), "dep", dep_code);
            k.set_defs(vec![k.load_def(&prov).unwrap(), k.load_def(&dep).unwrap()]);
            k.start().await.unwrap();

            let lua = k.lua().clone();
            let svc = k.service("svc").unwrap();
            lua.globals().set("__m7_4_svc", svc).unwrap();
            assert_eq!(lua.load("return __m7_4_svc.call()").eval::<String>().unwrap(), "v1:2");

            // New definition observes old instance already stopped, registers ctx-held resources
            // (a listener + disposer), mutates external state, and then fails in `apply`.
            let v2_failing = r#"return {
                name = "prov",
                provide = { "svc" },
                apply = function(ctx)
                    table.insert(mare._events, "new_apply:old_stopped=" .. tostring(mare._old_stopped))
                    ctx:on("probe/ping", function()
                        mare._new_pings = (mare._new_pings or 0) + 1
                    end)
                    ctx:effect(function() return function()
                        table.insert(mare._events, "new_dispose")
                    end end)
                    mare._external_side_effect = "mutated-before-fail"
                    error("new apply boom")
                end,
            }"#;
            write_bumped(prov.path.as_ref().unwrap(), v2_failing, 2);

            let report = k.reload_report(Some("prov")).await.unwrap();
            assert!(
                report.failures.iter().any(|f| f.contains("new apply boom")),
                "report.failures must include the apply error: {:?}",
                report.failures
            );
            assert!(
                report.applied.contains(&"prov".to_string()) && report.applied.contains(&"dep".to_string()),
                "re-enabled prov and dependent dep must be applied: {:?}",
                report.applied
            );

            // Prove exact lifecycle order: old instances dispose BEFORE new apply runs, failed new
            // apply cleans up its ctx-held resources, then old definition re-applies as fresh instance.
            let events: String = lua.load("return table.concat(mare._events, ',')").eval().unwrap();
            assert_eq!(
                events,
                "old_apply:1,dep_apply:1,dep_dispose:1,old_dispose:1,new_apply:old_stopped=true,new_dispose,old_apply:2,dep_apply:2"
            );

            // Failed new apply's ctx-held listener was cleaned up; only re-enabled old listener fires.
            k.emit("probe/ping", mlua::MultiValue::new()).await;
            let (old_pings, new_pings): (i64, Option<i64>) =
                lua.load("return mare._old_pings or 0, mare._new_pings").eval().unwrap();
            assert_eq!((old_pings, new_pings), (1, None));

            // Old instance was disposed; external side effect was NOT rolled back; old def re-applied
            // as a fresh instance (calls counter reset to 1, not continuing from 2).
            let (applies, disposes, dep_applies, dep_disposes, side_effect, dep_saw): (
                i64,
                i64,
                i64,
                i64,
                String,
                String,
            ) = lua
                .load(
                    "return mare._prov_applies, mare._prov_disposes, mare._dep_applies, mare._dep_disposes, mare._external_side_effect, mare._dep_saw",
                )
                .eval()
                .unwrap();
            assert_eq!((applies, disposes), (2, 1));
            assert_eq!((dep_applies, dep_disposes), (2, 1));
            assert_eq!(side_effect, "mutated-before-fail");
            assert_eq!(dep_saw, "v1:1", "re-enabled old definition must start as a fresh instance");

            let problems = k.problems().join("\n");
            assert!(
                problems.contains("new apply boom") && problems.contains("re-enabled"),
                "problems must record both error and re-enable: {problems}"
            );

            k.shutdown().await;
        })
        .await;
}

/// When both the new `apply` and the fallback re-enable of the
/// old definition fail, both failures are reported and the plugin remains inactive.
#[tokio::test]
async fn reload_apply_failure_when_reenable_also_fails_reports_both_and_stays_inactive() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let dir = tempfile::tempdir().unwrap();
            let v1 = r#"return {
                name = "prov",
                apply = function()
                    if mare._block_reenable then
                        error("old apply also boom")
                    end
                end,
            }"#;
            let prov = file_src(dir.path(), "prov", v1);
            k.set_defs(vec![k.load_def(&prov).unwrap()]);
            k.start().await.unwrap();
            assert!(crate::common::plugin_active(&k, "prov"));

            k.lua().load("mare._block_reenable = true").exec().unwrap();
            write_bumped(
                prov.path.as_ref().unwrap(),
                r#"return { name = "prov", apply = function() error("new apply boom") end }"#,
                2,
            );

            let report = k.reload_report(Some("prov")).await.unwrap();
            assert!(!crate::common::plugin_active(&k, "prov"), "plugin must not be active when both applies fail");
            assert!(report.applied.is_empty(), "nothing applied: {:?}", report.applied);
            let joined = report.failures.join("\n");
            assert!(
                joined.contains("new apply boom") && joined.contains("old apply also boom"),
                "both failures must be reported: {joined}"
            );

            k.shutdown().await;
        })
        .await;
}
