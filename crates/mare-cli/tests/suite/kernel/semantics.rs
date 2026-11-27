//! Kernel semantics through
//! public paths only: plugins are files in the user plugin directory, a plugin goes away when its
//! file is removed and the host reloads, and what is observed is what services do and what the
//! host reports. Tests of behavior not yet implemented are marked ignored;
//! the commit that lands the change removes the ignore.
//!
//! - single slot: a second provider of a name is refused (statically, naming both files; at run
//!   time, `has been registered`); nothing falls back to an earlier provider;
//! - a plugin whose `inject` has no provider waits (PENDING) and starts when one appears;
//! - registrations made through a `registration = true` method belong to the caller;
//! - cleanup order and errors, failed apply.

use super::{host, write_plugin};
use crate::common::*;

fn problems(h: &mare_host::Host) -> String {
    h.kernel.problems().join("\n")
}

#[tokio::test]
async fn mare_lib_caller_identity_uses_native_fiber() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let (name, id, no_name, no_id): (String, i64, bool, bool) = eval(
            &h,
            r#"local lib = require("mare_lib")
               local caller = { name = "old-name", id = 17, fiber = { name = "native-name", uid = 42 } }
               return lib.caller_name(caller), lib.caller_id(caller),
                   lib.caller_name(nil) == nil and lib.caller_name({}) == nil,
                   lib.caller_id(nil) == nil and lib.caller_id({}) == nil"#,
        )
        .await;
        assert_eq!(name, "native-name");
        assert_eq!(id, 42);
        assert!(no_name && no_id, "missing callers must have no identity");
        h.shutdown().await;
    })
    .await
}

/// The `echo` service's contract (a user plugin's service needs one).
macro_rules! echo_contract {
    () => {
        r#"{ contract = { methods = { who = { doc = "who answers", expose = "lua",
             params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "string" } } } } }"#
    };
}

const ECHO_A: &str = concat!(
    r#"local ECHO_CONTRACT = "#,
    echo_contract!(),
    r#"
return { name = "echo-a", provide = { "echo" }, apply = function(ctx)
  local Echo = require("cordis").class("Echo", mare.Service)
  function Echo:constructor(c) Echo.super.constructor(self, c, "echo", ECHO_CONTRACT) end
  function Echo:who() return "a" end
  ctx:plugin(Echo)
end }"#
);

const ECHO_B: &str = concat!(
    r#"local ECHO_CONTRACT = "#,
    echo_contract!(),
    r#"
return { name = "echo-b", provide = { "echo" }, apply = function(ctx)
  local Echo = require("cordis").class("Echo", mare.Service)
  function Echo:constructor(c) Echo.super.constructor(self, c, "echo", ECHO_CONTRACT) end
  function Echo:who() return "b" end
  ctx:plugin(Echo)
end }"#
);

/// Two enabled definitions provide the same name: the loader refuses the later one before it runs
/// and names both files; the first keeps the slot.
// guarantee: one-provider-per-service-name
#[tokio::test]
async fn a_second_provider_of_a_name_is_refused_naming_both_files() {
    local(async {
        let env = TestEnv::new().await;
        let a = write_plugin(&env, "echo-a", ECHO_A);
        let b = write_plugin(&env, "echo-b", ECHO_B);
        let h = host(&env).await;
        let who: String = eval(&h, r#"return C.cordis:get("echo"):who()"#).await;
        let p = problems(&h);
        h.shutdown().await;
        assert_eq!(who, "a", "the first in load order keeps the slot");
        for f in [&a, &b] {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            assert!(p.contains(&name), "the problem names {name}: {p}");
        }
        assert!(p.contains("already provided"), "{p}");
    })
    .await
}

const LATE_PROVIDER: &str = concat!(
    r#"local ECHO_CONTRACT = "#,
    echo_contract!(),
    r#"
return { name = "late", apply = function(ctx)
  -- one attempt: a full reload restarts every plugin (520e66ea), and a second apply taking the
  -- freed slot is a new provide, not the fallback this test rules out
  if mare._late ~= nil then return end
  local Echo = require("cordis").class("Echo", mare.Service)
  function Echo:constructor(c) Echo.super.constructor(self, c, "echo", ECHO_CONTRACT) end
  function Echo:who() return "late" end
  -- the rows start concurrently: the attempt is made once echo is provided, so it is a run-time
  -- provide into a taken slot (refused, coded), never a race for an empty one
  ctx:inject({ "echo" }, function(sub)
    if mare._late ~= nil then return end
    local ok, err = pcall(Echo.new, sub)
    mare._late = ok and "provided" or (type(err) == "table" and err.code) or tostring(err)
  end)
end }"#
);

/// A provide at run time into a taken slot is refused with the error code `already_provided`, the
/// holder keeps answering, and when the holder goes away nothing falls back: the name has no
/// provider until a plugin provides it again (no provider stack).
// guarantee: no-fallback-when-a-provider-leaves
#[tokio::test]
async fn a_taken_slot_refuses_a_second_provide_and_nothing_falls_back() {
    local(async {
        let env = TestEnv::new().await;
        let a = write_plugin(&env, "echo-a", ECHO_A);
        write_plugin(&env, "late", LATE_PROVIDER);
        let h = host(&env).await;
        let (late, who): (String, String) = eval(&h, r#"return mare._late, C.cordis:get("echo"):who()"#).await;
        assert_eq!(late, "already_provided", "the refusal is the structured error code");
        assert_eq!(who, "a");
        std::fs::remove_file(&a).unwrap();
        h.reload_all().await.unwrap();
        let gone: bool = eval(&h, r#"return C.cordis:get("echo") == nil"#).await;
        h.shutdown().await;
        assert!(gone, "a refused provide must not take over when the holder goes away");
    })
    .await
}

/// A plugin that was running and is
/// reloaded onto a definition whose inject nobody provides is left PENDING, not failed and not put
/// back on its old definition; the reload's notices name what it waits for (what `/reload` shows),
/// and it starts on the new definition once a provider appears.
// guarantee: dependent-waits-while-its-provider-is-gone
#[tokio::test]
async fn a_running_plugin_reloaded_onto_a_missing_inject_waits_and_resumes_with_its_provider() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "needs", r#"return { name = "needs", apply = function(ctx) mare._needs = "v1" end }"#);
        let h = host(&env).await;
        assert_eq!(eval::<Option<String>>(&h, "return mare._needs").await.as_deref(), Some("v1"));

        exec(&h, "mare._needs = nil").await;
        write_plugin(
            &env,
            "needs",
            r#"return { name = "needs", inject = { "echo" }, apply = function(ctx) mare._needs = "v2:" .. ctx.echo:who() end }"#,
        );
        let out = h.reload_all().await.unwrap();
        let notices = out["notices"].to_string();
        assert!(notices.contains("plugin needs is waiting for echo"), "the reload names what it waits for: {notices}");
        assert!(!notices.contains("failed"), "waiting is not a failure: {notices}");
        assert_eq!(
            eval::<Option<String>>(&h, "return mare._needs").await,
            None,
            "neither the new definition (no provider) nor the old one (not a failure) runs"
        );

        write_plugin(&env, "echo-a", ECHO_A);
        h.reload_all().await.unwrap();
        let after: Option<String> = eval(&h, "return mare._needs").await;
        h.shutdown().await;
        assert_eq!(after.as_deref(), Some("v2:a"), "it starts on the new definition once the provider is there");
    })
    .await
}

const NEEDS_ECHO: &str = r#"return { name = "needs-echo", inject = { "echo" }, apply = function(ctx)
  mare._needs = ctx.echo:who()
end }"#;

/// When two dependent plugins whose new definitions both fail in one reload are restored, the
/// dependent waits (PENDING) while its provider is down, starts on its new definition once the
/// provider is put back, fails, and is put back too: both run on their previous definitions (each
/// restore reads live state rather than a snapshot taken before any restore).
// guarantee: dependents-put-back-with-their-provider
#[tokio::test]
async fn a_dependent_that_fails_once_its_provider_is_put_back_is_put_back_too() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "echo-a", ECHO_A);
        write_plugin(&env, "needs-echo", NEEDS_ECHO);
        let h = host(&env).await;
        assert_eq!(eval::<Option<String>>(&h, "return mare._needs").await.as_deref(), Some("a"));

        exec(&h, "mare._needs = nil").await;
        write_plugin(
            &env,
            "echo-a",
            r#"return { name = "echo-a", provide = { "echo" }, apply = function(ctx) error("echo v2 broken") end }"#,
        );
        write_plugin(
            &env,
            "needs-echo",
            r#"return { name = "needs-echo", inject = { "echo" }, apply = function(ctx) error("needs v2 broken") end }"#,
        );
        let out = h.reload_all().await.unwrap();
        let notices = out["notices"].to_string();
        let (who, needs): (Option<String>, Option<String>) =
            eval(&h, r#"local e = C.cordis:get("echo") return e and e:who(), mare._needs"#).await;
        let active = (plugin_active(&h.kernel, "echo-a"), plugin_active(&h.kernel, "needs-echo"));
        h.shutdown().await;
        assert!(notices.contains("echo v2 broken") && notices.contains("needs v2 broken"), "{notices}");
        assert_eq!(who.as_deref(), Some("a"), "the provider runs on its previous definition");
        assert_eq!(needs.as_deref(), Some("a"), "the dependent runs on its previous definition: {notices}");
        assert_eq!(active, (true, true));
    })
    .await
}

/// A plugin whose `inject` has no provider is not a failure: it waits, the host reports what it
/// waits for, and it starts as soon as a provider appears (PENDING).
// guarantee: inject-waits-instead-of-failing
#[tokio::test]
async fn a_plugin_missing_its_inject_waits_and_starts_when_the_provider_appears() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "needs-echo", NEEDS_ECHO);
        let h = host(&env).await;
        let before: Option<String> = eval(&h, "return mare._needs").await;
        assert_eq!(before, None, "it must not run without its provider");
        assert!(!problems(&h).contains("failed"), "waiting is not a failure: {}", problems(&h));
        write_plugin(&env, "echo-a", ECHO_A);
        h.reload_all().await.unwrap();
        let after: Option<String> = eval(&h, "return mare._needs").await;
        h.shutdown().await;
        assert_eq!(after.as_deref(), Some("a"), "it starts once the provider is there");
    })
    .await
}

const REG_PROVIDER: &str = r#"return { name = "reg-svc", provide = { "reg" }, apply = function(ctx)
  mare._items = mare._items or {}
  local Reg = require("cordis").class("Reg", mare.Service)
  function Reg:constructor(c)
    Reg.super.constructor(self, c, "reg", { contract = { methods = { add = { doc = "add one item", expose = "lua", registration = true,
      params = { type = "object", properties = { item = { type = "string" } }, required = { "item" },
        additionalProperties = false }, order = { "item" } } } } })
  end
  -- the registration is an effect of the caller's fiber: ownership is the caller's, the returned
  -- release is its disposer
  function Reg:add(item)
    local release = self.ctx:effect(function()
      mare._items[item] = (mare._items[item] or 0) + 1
      return function()
        mare._items[item] = mare._items[item] - 1
        mare._released = mare._released or {}
        mare._released[item] = (mare._released[item] or 0) + 1
      end
    end, "reg item")
    return function() release() end
  end
  ctx:plugin(Reg)
end }"#;

const REG_USER: &str = r#"return { name = "reg-user", inject = { "reg" }, apply = function(ctx)
  mare._proxy = ctx.reg
  mare._release = ctx.reg:add("held")
  ctx.reg:add("kept")
end }"#;

/// Registration ownership (Cordis TS INACTIVE_EFFECT): the release a caller gets is
/// idempotent; what it still holds is released when it goes away (the provider ties it to the
/// caller with `self.ctx:effect`; the guard wraps nothing); an explicit release is not released again; a proxy kept past its
/// owner's end is refused (`inactive_ctx`) before the provider runs.
// guarantee: registration-withdrawn-on-caller-unload
#[tokio::test]
async fn a_callers_registrations_are_its_own_and_end_with_it() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "reg-svc", REG_PROVIDER);
        write_plugin(&env, "reg-user", REG_USER);
        let h = host(&env).await;
        let count = |item: &'static str| {
            let h = h.clone();
            async move { eval::<i64>(&h, &format!("return mare._items['{item}'] or 0")).await }
        };
        assert_eq!((count("held").await, count("kept").await), (1, 1), "{}", problems(&h));
        exec(&h, "mare._release() mare._release()").await;
        assert_eq!(count("held").await, 0, "A1: the release is idempotent");
        // only the caller goes (a rows-driven unload): the provider keeps running, so a registration
        // wrongly attributed to the provider would survive
        unload_plugin(&h.kernel, "reg-user").await;
        assert!(plugin_active(&h.kernel, "reg-svc"), "the provider must keep running");
        let released = |item: &'static str| {
            let h = h.clone();
            async move { eval::<Option<i64>>(&h, &format!("return mare._released and mare._released['{item}']")).await }
        };
        assert_eq!(released("kept").await, Some(1), "A2: the disposer of the kept registration ran exactly once");
        assert_eq!(released("held").await, Some(1), "A3: the released registration's disposer ran only once");
        assert_eq!(count("kept").await, 0, "A2: the registration outlived its owner");
        assert_eq!(count("held").await, 0, "A3: a released registration was released again");
        let refused: String = eval(
            &h,
            "local ok, e = pcall(mare._proxy.add, mare._proxy, 'late') return ok and 'ok' or tostring(e.code or e)",
        )
        .await;
        let late = count("late").await;
        h.shutdown().await;
        assert_eq!(refused, "inactive_ctx", "A4: a proxy past its owner registers nothing");
        assert_eq!(late, 0, "A4: the provider must not have run");
    })
    .await
}

const PROVIDER_GOES: &str = r#"return { name = "reg-user2", inject = { "reg" }, apply = function(ctx)
  mare._release2 = ctx.reg:add("x")
end }"#;

/// Reference (Cordis TS tests/service.spec.ts:190/202/216): the provider goes away first; the
/// release its former caller holds does nothing and never calls into the unloaded provider.
// guarantee: release-after-provider-is-gone-does-nothing
#[tokio::test]
async fn a_release_after_its_provider_is_gone_does_nothing() {
    local(async {
        let env = TestEnv::new().await;
        let svc = write_plugin(&env, "reg-svc", REG_PROVIDER);
        write_plugin(&env, "reg-user2", PROVIDER_GOES);
        let h = host(&env).await;
        let held: i64 = eval(&h, "return mare._items.x").await;
        assert_eq!(held, 1, "{}", problems(&h));
        std::fs::remove_file(&svc).unwrap();
        h.reload_all().await.unwrap();
        // the provider's own fiber is gone; its disposer must not run again from the old release
        exec(&h, "mare._items.x = 100").await;
        let ok: bool = eval(&h, "return (pcall(mare._release2))").await;
        let after: i64 = eval(&h, "return mare._items.x").await;
        h.shutdown().await;
        assert!(ok, "the stale release must not raise");
        assert_eq!(after, 100, "the stale release called into the unloaded provider");
    })
    .await
}

/// A `CordisError("INACTIVE_EFFECT")` raised inside a service
/// method normalizes to `inactive_ctx`, and `inactive_ctx` stays `inactive_ctx` across both the
/// Lua guard and the JSON export bridge instead of collapsing to `internal`.
#[tokio::test]
async fn inactive_effect_and_inactive_ctx_keep_code_across_lua_and_export_calls() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(
            &env,
            "reg-err-victim",
            r#"return { name = "reg-err-victim", apply = function(ctx)
              mare._disposed_ctx = ctx
            end }"#,
        );
        write_plugin(
            &env,
            "reg-err-svc",
            r#"return { name = "reg-err-svc", provide = { "reg_err" },
              contracts = { reg_err = { methods = {
                stale = { doc = "raise inactive_ctx", expose = "both", result = { type = "object" } },
                cordis = { doc = "effect on a disposed ctx", expose = "both", result = { type = "object" } },
              } } },
              apply = function(ctx)
                local S = require("cordis").class("RegErr", mare.Service)
                function S:constructor(c)
                  S.super.constructor(self, c, "reg_err")
                end
                function S:stale()
                  error({ code = "inactive_ctx", message = "caller ctx disposed" })
                end
                function S:cordis()
                  mare._disposed_ctx:effect(function() end)
                  return {}
                end
                ctx:plugin(S)
              end }"#,
        );
        write_plugin(
            &env,
            "reg-err-unloading",
            r#"return { name = "reg-err-unloading", inject = { "reg_err" }, apply = function(ctx)
              ctx:effect(function()
                return function()
                  local ok, e = pcall(ctx.reg_err.cordis, ctx.reg_err)
                  mare._unloading_err = ok and "ok" or tostring(e and e.code or e)
                end
              end, "unloading check")
            end }"#,
        );
        let h = host(&env).await;
        unload_plugin(&h.kernel, "reg-err-victim").await;
        let caller_id = h.kernel.open_owner("<root>", "root").unwrap();
        let (lua_stale, lua_cordis): (String, String) = eval(
            &h,
            r#"local svc = C.cordis:get("reg_err")
               local _, e1 = pcall(svc.stale, svc)
               local _, e2 = pcall(svc.cordis, svc)
               return tostring(e1 and e1.code or e1), tostring(e2 and e2.code or e2)"#,
        )
        .await;
        // reference implementation: UNLOADING cleanup registering an effect sees inactive_ctx.
        unload_plugin(&h.kernel, "reg-err-unloading").await;
        let unloading_code: String = eval(&h, "return tostring(mare._unloading_err)").await;
        let rpc_stale = h
            .kernel
            .call_service_export(caller_id, None, "reg_err", "stale", serde_json::json!({}), Default::default())
            .await
            .unwrap_err()
            .code()
            .to_owned();
        let rpc_cordis = h
            .kernel
            .call_service_export(caller_id, None, "reg_err", "cordis", serde_json::json!({}), Default::default())
            .await
            .unwrap_err()
            .code()
            .to_owned();
        h.shutdown().await;
        assert_eq!(lua_stale, "inactive_ctx");
        assert_eq!(lua_cordis, "inactive_ctx");
        assert_eq!(unloading_code, "inactive_ctx");
        assert_eq!(rpc_stale, "inactive_ctx");
        assert_eq!(rpc_cordis, "inactive_ctx");
    })
    .await
}

const WATCHER: &str = r#"return { name = "watcher", apply = function(ctx)
  mare._errors = mare._errors or {}
  ctx:on("kernel/error", function(ev) table.insert(mare._errors, tostring(ev.message)) end)
end }"#;

const CLEANUP: &str = r#"return { name = "cleanup", apply = function(ctx)
  mare._order = {}
  ctx:effect(function() return function() mare._order[#mare._order + 1] = "first" end end)
  ctx:effect(function() return function() error("CLEANUP-BOOM") end end)
  ctx:effect(function() return function() mare._order[#mare._order + 1] = "third" end end)
end }"#;

/// Cleanup errors (Cordis TS tests/dispose.spec.ts:205/217/230/242):
/// cleanups run last-in first-out, each exactly once; one that raises does not stop the others,
/// and its error is reported once.
// guarantee: effects-dispose-in-reverse-start-order
// guarantee: cleanup-error-does-not-stop-the-rest
#[tokio::test]
async fn cleanups_run_last_in_first_out_and_one_error_does_not_stop_the_rest() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "watcher", WATCHER);
        let f = write_plugin(&env, "cleanup", CLEANUP);
        let h = host(&env).await;
        std::fs::remove_file(&f).unwrap();
        h.reload_all().await.unwrap();
        let (order, errors): (Vec<String>, Vec<String>) = eval(&h, "return mare._order, mare._errors").await;
        h.shutdown().await;
        assert_eq!(order, ["third", "first"], "last in, first out; the raising one does not stop the rest");
        let boom = errors.iter().filter(|e| e.contains("CLEANUP-BOOM")).count();
        assert_eq!(boom, 1, "the error is reported once (kernel/error): {errors:?}");
    })
    .await
}

const FAILS: &str = r#"return { name = "fails", provide = { "half" }, apply = function(ctx)
  local Half = require("cordis").class("Half", mare.Service)
  function Half:constructor(c)
    Half.super.constructor(self, c, "half",
      { contract = { methods = { x = { doc = "x", expose = "lua", params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "integer" } } } } })
  end
  function Half:x() return 1 end
  ctx:plugin(Half)
  ctx:effect(function() return function() mare._rolled_back = true end end)
  error("APPLY-BOOM")
end }"#;

/// Reference (Cordis lifecycle semantics): a failed apply is rolled back — what it
/// provided and registered is gone, its cleanups ran — before the failure is reported.
// guarantee: failed-apply-rolled-back-before-it-is-reported
#[tokio::test]
async fn a_failed_apply_is_rolled_back_before_it_is_reported() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "fails", FAILS);
        let h = host(&env).await;
        let (rolled, gone): (bool, bool) =
            eval(&h, r#"return mare._rolled_back == true, C.cordis:get("half") == nil"#).await;
        let p = problems(&h);
        h.shutdown().await;
        assert!(p.contains("APPLY-BOOM"), "the failure is reported: {p}");
        assert!(rolled, "its cleanups ran");
        assert!(gone, "what it provided is gone");
    })
    .await
}

/// A user plugin's own `usage`: the built-in contract's seven methods, answering a marker.
const MY_USAGE: &str = r#"return { name = "my-usage", provide = { "usage" }, apply = function(ctx)
  local Usage = require("cordis").class("Usage", mare.Service)
  function Usage:constructor(c) Usage.super.constructor(self, c, "usage") end
  function Usage:get() return {} end
  function Usage:totals() return {} end
  function Usage:context_tokens() return 4242 end
  function Usage:set_context() end
  function Usage:cost_text() return "" end
  function Usage:rate_limits() return {} end
  function Usage:quota_lines() return {} end
  ctx:plugin(Usage)
end }"#;

/// The two ways to replace a built-in service: `override = true` (kept by
/// plugins/overrides.rs) and turning the built-in off, then providing the name yourself. This is
/// the second; it holds before and after the switch.
#[tokio::test]
async fn turning_a_builtin_off_lets_a_plugin_provide_its_service() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.plugin('usage', { enabled = false })\n").unwrap();
        write_plugin(&env, "my-usage", MY_USAGE);
        let b = env.host().user_config(false).no_session();
        let h = mare_host::Host::new(b.opts).await.unwrap();
        expose(&h);
        let tokens: i64 = eval(&h, r#"return C.cordis:get("usage"):context_tokens()"#).await;
        let p = problems(&h);
        h.shutdown().await;
        assert_eq!(tokens, 4242, "the plugin provides the turned-off built-in's service: {p}");
    })
    .await
}

/// Without turning the built-in off, the same plugin is a second provider of `usage`: refused
/// before it runs, naming both, and the built-in keeps the slot. (Today the user plugin shadows
/// the built-in.)
#[tokio::test]
async fn a_plugin_cannot_take_a_builtins_service_without_turning_it_off() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "my-usage", MY_USAGE);
        let h = env.host().builtins(&["usage"]).user_plugins().no_session().build_unchecked_exposed().await;
        let tokens: i64 = eval(&h, r#"return C.cordis:get("usage"):context_tokens()"#).await;
        let p = problems(&h);
        h.shutdown().await;
        assert_ne!(tokens, 4242, "the built-in keeps the slot");
        assert!(p.contains("my-usage") && p.contains("already provided"), "{p}");
    })
    .await
}

const CYCLE_A: &str = r#"return { name = "cyc-a", provide = { "x" }, inject = { "y" },
  apply = function(ctx) require("mare_lib").service(ctx, "x", {}) end }"#;
const CYCLE_B: &str = r#"return { name = "cyc-b", provide = { "y" }, inject = { "x" },
  apply = function(ctx) require("mare_lib").service(ctx, "y", {}) end }"#;
const INDEPENDENT: &str = r#"return { name = "indep", apply = function() mare._indep = true end }"#;

/// An inject cycle (two plugins each needing a service only the other provides) is refused
/// statically, as a second provider is (coordinator's ruling; Cordis would leave
/// both PENDING for good): neither member runs, the problem names the cycle, and an independent
/// plugin still starts normally. (Was kernel/lifecycle.rs cycles_only_fail_their_members.)
// guarantee: inject-cycle-refused-and-the-rest-starts
#[tokio::test]
async fn an_inject_cycle_is_refused_statically_and_the_rest_starts() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "cyc-a", CYCLE_A);
        write_plugin(&env, "cyc-b", CYCLE_B);
        write_plugin(&env, "indep", INDEPENDENT);
        let h = host(&env).await;
        let (indep, x, y): (bool, bool, bool) =
            eval(&h, r#"return mare._indep == true, C.cordis:get("x") ~= nil, C.cordis:get("y") ~= nil"#).await;
        let p = problems(&h);
        let active =
            (plugin_active(&h.kernel, "indep"), plugin_active(&h.kernel, "cyc-a"), plugin_active(&h.kernel, "cyc-b"));
        h.shutdown().await;
        assert!(indep, "an independent plugin starts: {p}");
        assert_eq!(active, (true, false, false), "only the independent plugin is active: {p}");
        assert!(!x && !y, "neither member of the cycle runs");
        for member in ["cyc-a", "cyc-b"] {
            assert!(
                p.lines().any(|l| l.contains(member) && l.contains("not loaded: dependency cycle")),
                "{member} must be refused with the cycle diagnostic: {p}"
            );
        }
        assert!(!p.lines().any(|l| l.contains("indep")), "the independent plugin has no problem: {p}");
    })
    .await
}

/// a reload by name reports only after the restarts it set off have
/// settled. Replacing a provider restarts its dependent after `loader:replace` returns; a dependent
/// whose new start fails is in the reload's failures, not left FAILED behind an empty report.
#[tokio::test]
async fn a_reload_by_name_reports_a_dependent_that_fails_after_the_provider_is_replaced() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "echo-a", ECHO_A);
        write_plugin(
            &env,
            "slow-dep",
            r#"return { name = "slow-dep", inject = { "echo" }, apply = function(ctx)
  mare._slow_applies = (mare._slow_applies or 0) + 1
  if mare._slow_applies > 1 then
    mare.task.sleep(30)
    error("slow-dep restart failed")
  end
end }"#,
        );
        let h = host(&env).await;
        assert!(plugin_active(&h.kernel, "slow-dep"), "{}", problems(&h));
        let report = h.kernel.reload_report(Some("echo-a")).await.unwrap();
        let failures = report.failures.join("\n");
        let applies: i64 = eval(&h, "return mare._slow_applies").await;
        h.shutdown().await;
        assert_eq!(applies, 2, "the dependent restarted with its provider");
        assert!(failures.contains("slow-dep"), "the dependent's failed restart is reported: {failures:?}");
    })
    .await
}

/// a plugin left PENDING (its inject had no provider) whose new
/// definition drops the inject starts on a reload by name, and counts as applied (cordis
/// replace.spec.ts:79: a new definition removing a missing dependency).
#[tokio::test]
async fn a_pending_plugin_started_by_a_reload_by_name_counts_as_applied() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "late-start", r#"return { name = "late-start", inject = { "nobody-provides" }, apply = function(ctx) mare._late_started = true end }"#);
        let h = host(&env).await;
        assert!(!plugin_active(&h.kernel, "late-start"), "it waits for its inject");
        write_plugin(&env, "late-start", r#"return { name = "late-start", apply = function(ctx) mare._late_started = true end }"#);
        let report = h.kernel.reload_report(Some("late-start")).await.unwrap();
        let started: bool = eval(&h, "return mare._late_started == true").await;
        let active = plugin_active(&h.kernel, "late-start");
        h.shutdown().await;
        assert!(started && active);
        assert_eq!(report.applied, vec!["late-start".to_string()], "pending: {:?}, failures: {:?}", report.pending, report.failures);
    })
    .await
}

/// a directory plugin's lua/ dir is registered when its row is built.
/// A row that never applied (PENDING) has no owner cleanup to drop it, so a reload that removes
/// that row drops it.
#[tokio::test]
async fn a_removed_pending_row_drops_its_module_dir() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("config/plugins/pend-dir");
        std::fs::create_dir_all(dir.join("lua")).unwrap();
        std::fs::write(
            dir.join("init.lua"),
            r#"return { name = "pend-dir", inject = { "nobody-provides" }, apply = function(ctx) end }"#,
        )
        .unwrap();
        std::fs::write(dir.join("lua/pend_dir_mod.lua"), "return {}").unwrap();
        let h = host(&env).await;
        let before = h.kernel.module_dir_registered("pend-dir");
        std::fs::remove_dir_all(&dir).unwrap();
        h.reload_all().await.unwrap();
        let after = h.kernel.module_dir_registered("pend-dir");
        h.shutdown().await;
        assert!(before, "the PENDING row's lua/ dir is registered");
        assert!(!after, "the removed row's lua/ dir is still registered with the module searcher");
    })
    .await
}

/// A user plugin that replaces a built-in (`override = true`) leaves the
/// built-in's declaration as a refused row of the same name in the loader's dump. That row has no
/// state and must not erase the running row's: when an edit of the replacement fails, the reload
/// puts its previous definition back.
#[tokio::test]
async fn an_overriding_plugin_whose_edit_fails_is_put_back_despite_the_refused_builtin_row() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(
            &env,
            "update-check",
            r#"return { name = "update-check", override = true, provide = { "update-check" }, apply = function(ctx)
  mare._uc_v1 = (mare._uc_v1 or 0) + 1
end }"#,
        );
        let h = host(&env).await;
        assert!(plugin_active(&h.kernel, "update-check"), "{}", problems(&h));
        write_plugin(
            &env,
            "update-check",
            r#"return { name = "update-check", override = true, provide = { "update-check" }, apply = function(ctx) error("uc v2 broken") end }"#,
        );
        let out = h.reload_all().await.unwrap();
        let v1: i64 = eval(&h, "return mare._uc_v1").await;
        let active = plugin_active(&h.kernel, "update-check");
        h.shutdown().await;
        assert!(active, "the replacement runs on its previous definition: {out}");
        assert_eq!(v1, 2, "its previous definition applied again");
    })
    .await
}

const Z_ECHO_V1: &str = concat!(
    r#"local ECHO_CONTRACT = "#,
    echo_contract!(),
    r#"
return { name = "z-echo", provide = { "echo" }, apply = function(ctx)
  local Echo = require("cordis").class("Echo", mare.Service)
  function Echo:constructor(c) Echo.super.constructor(self, c, "echo", ECHO_CONTRACT) end
  function Echo:who() return "z" end
  ctx:plugin(Echo)
end }"#
);

/// When a provider's new definition fails and is rolled back, a dependent
/// whose new definition also fails is put back too, also when it fails only after an await (it is
/// still starting when the provider's restore returns) and when it is listed before the provider
/// (it was passed over while it waited).
async fn dependent_put_back_after_its_provider(dependent: &str, v2: &str) {
    let env = TestEnv::new().await;
    write_plugin(&env, "z-echo", Z_ECHO_V1);
    write_plugin(
        &env,
        dependent,
        &format!(
            r#"return {{ name = "{dependent}", inject = {{ "echo" }}, apply = function(ctx) mare._dep = ctx.echo:who() end }}"#
        ),
    );
    let h = host(&env).await;
    assert_eq!(eval::<Option<String>>(&h, "return mare._dep").await.as_deref(), Some("z"), "{}", problems(&h));
    exec(&h, "mare._dep = nil").await;
    write_plugin(
        &env,
        "z-echo",
        r#"return { name = "z-echo", provide = { "echo" }, apply = function(ctx) error("z-echo v2 broken") end }"#,
    );
    write_plugin(
        &env,
        dependent,
        &format!(r#"return {{ name = "{dependent}", inject = {{ "echo" }}, apply = function(ctx) {v2} end }}"#),
    );
    let out = h.reload_all().await.unwrap();
    let dep: Option<String> = eval(&h, "return mare._dep").await;
    let active = (plugin_active(&h.kernel, "z-echo"), plugin_active(&h.kernel, dependent));
    h.shutdown().await;
    assert_eq!(active, (true, true), "{out}");
    assert_eq!(dep.as_deref(), Some("z"), "the dependent runs on its previous definition: {out}");
}

#[tokio::test]
async fn a_dependent_that_fails_after_an_await_is_put_back_after_its_provider() {
    local(dependent_put_back_after_its_provider("needs-z", r#"mare.task.sleep(30) error("needs-z v2 broken")"#)).await;
}

#[tokio::test]
async fn a_dependent_listed_before_its_provider_is_put_back_after_it() {
    local(dependent_put_back_after_its_provider("a-needs-z", r#"error("a-needs-z v2 broken")"#)).await;
}

/// Review a-reload2 (should-fix): a PENDING directory plugin turned off (`enabled = false`) never
/// applied, so no owner cleanup drops the lua/ dir its row registered; the reload does.
#[tokio::test]
async fn a_pending_row_turned_off_drops_its_module_dir() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.config_dir().join("plugins/pend-off");
        std::fs::create_dir_all(dir.join("lua")).unwrap();
        std::fs::write(
            dir.join("init.lua"),
            r#"return { name = "pend-off", inject = { "nobody-provides" }, apply = function(ctx) end }"#,
        )
        .unwrap();
        std::fs::write(dir.join("lua/pend_off_mod.lua"), "return {}").unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "").unwrap();
        // a PENDING plugin is a startup problem: Host::new, not the builder's checked build()
        let h = mare_host::Host::new(env.host().user_config(false).no_session().opts).await.unwrap();
        expose(&h);
        let before = h.kernel.module_dir_registered("pend-off");
        std::fs::write(env.config_dir().join("init.lua"), r#"mare.plugin("pend-off", { enabled = false })"#).unwrap();
        h.reload_config().await.unwrap();
        let after = h.kernel.module_dir_registered("pend-off");
        h.shutdown().await;
        assert!(before, "the PENDING row's lua/ dir is registered");
        assert!(!after, "the turned-off row's lua/ dir is still registered");
    })
    .await
}

/// Review a-reload2 (should-fix): a plugin removed for good is not what a later plugin of the same
/// name falls back to when that one's first apply fails.
#[tokio::test]
async fn a_removed_plugin_is_not_the_rollback_of_a_new_one_with_its_name() {
    local(async {
        let env = TestEnv::new().await;
        let path = write_plugin(
            &env,
            "addon",
            r#"return { name = "addon", apply = function(ctx) mare._addon_old = (mare._addon_old or 0) + 1 end }"#,
        );
        let h = host(&env).await;
        assert!(plugin_active(&h.kernel, "addon"));
        std::fs::remove_file(&path).unwrap();
        h.reload_all().await.unwrap();
        assert!(!plugin_active(&h.kernel, "addon"));
        write_plugin(
            &env,
            "addon",
            r#"return { name = "addon", apply = function(ctx) error("new addon broken") end }"#,
        );
        let out = h.reload_all().await.unwrap();
        let old: i64 = eval(&h, "return mare._addon_old").await;
        let active = plugin_active(&h.kernel, "addon");
        h.shutdown().await;
        assert!(!active, "the new plugin failed; nothing of the removed one runs: {out}");
        assert_eq!(old, 1, "the removed plugin's definition was started again");
    })
    .await
}

#[tokio::test]
async fn a_plugin_providing_through_ctx_plugin_serves_immediately_at_startup() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo' }\nmare.plugin('llm', { enabled = false })\n",
        )
        .unwrap();
        write_plugin(
            &env,
            "my-extra",
            r#"return {
  name = "my-extra",
  provide = { "extra" },
  contracts = { extra = { methods = { ping = { doc = "ping", expose = "lua", params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "string" } } } } },
  apply = function(ctx)
    local S = require("cordis").class("S", mare.Service)
    function S:constructor(c) S.super.constructor(self, c, "extra") end
    function S:ping() return "pong" end
    ctx:plugin(S)
  end,
}"#,
        );
        // stand-in for built-in `llm` implementing all required methods of the built-in `llm` contract
        let lua = mlua::Lua::new();
        let val: mlua::Value = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
        let contracts = mare_kernel::contracts::parse_contracts(&lua, val).unwrap();
        let llm_methods = contracts
            .get("llm")
            .unwrap()
            .methods
            .iter()
            .filter(|(_, spec)| !spec.optional)
            .map(|(m, _)| format!("'{m}'"))
            .collect::<Vec<_>>()
            .join(", ");
        write_plugin(
            &env,
            "my-llm",
            &format!(
                "return {{ name = 'my-llm', provide = {{ 'llm' }}, apply = function(ctx)\n\
                 local S = require('cordis').class('S', mare.Service)\n\
                 function S:constructor(c) S.super.constructor(self, c, 'llm') end\n\
                 for _, m in ipairs({{ {llm_methods} }}) do S[m] = function() end end\n\
                 ctx:plugin(S) end }}"
            ),
        );
        let opts = env.host().user_config(false).no_session().opts;
        let h = mare_host::Host::new(opts).await.unwrap();
        // Immediate synchronous check before any Lua eval yields to Tokio.
        let has_extra = h.service_table("extra").is_some();
        let has_llm = h.service_table("llm").is_some();
        let tree = h.kernel.plugin_tree().unwrap();
        let extra_row = tree.into_iter().find(|r| r.id == "my-extra").unwrap();
        let agent_active = plugin_active(&h.kernel, "agent");
        let p = problems(&h);
        assert!(has_extra, "extra service must be registered before Host::new returns: {p}");
        assert!(extra_row.provides.iter().any(|s| s == "extra"), "tree row reports provided service: {:?}", extra_row.provides);
        assert!(has_llm, "llm replacement must be registered before Host::new returns: {p}");
        assert!(agent_active, "dependents of llm must start once llm is provided: {p}");
        assert!(!p.contains("required service `llm` is not available"), "no false pending warning: {p}");

        h.reload_all().await.unwrap();
        let has_extra_after = h.service_table("extra").is_some();
        let has_llm_after = h.service_table("llm").is_some();
        let agent_after = plugin_active(&h.kernel, "agent");
        let p_after = problems(&h);
        h.shutdown().await;
        assert!(has_extra_after, "extra service stays registered after reload: {p_after}");
        assert!(has_llm_after, "llm service must still be provided after reload: {p_after}");
        assert!(agent_after, "dependents of llm stay active after reload: {p_after}");
        assert!(!p_after.contains("required service `llm` is not available"), "no false pending warning after reload: {p_after}");
    })
    .await
}

#[tokio::test]
async fn a_failing_ctx_plugin_service_fails_its_owning_plugin_and_rolls_back_on_reload() {
    local(async {
        {
            let env = TestEnv::new().await;
            // 1. Failing ctx:plugin(S) (both undeclared sync S and async S:start) rolls back
            // parent effects/listeners and fails the owning row.
            write_plugin(
                &env,
                "undecl",
                r#"return {
  name = "undecl",
  apply = function(ctx)
    ctx:effect(function()
      return function() mare.g.undecl_cleaned = true end
    end)
    ctx:on("probe/undecl", function() return "alive" end)
    local S = require("cordis").class("S", mare.Service)
    function S:constructor(c) S.super.constructor(self, c, "extra") end
    ctx:plugin(S)
  end,
}"#,
            );
            write_plugin(
                &env,
                "async-child",
                r#"return {
  name = "async-child",
  apply = function(ctx)
    ctx:effect(function()
      return function() mare.g.async_cleaned = true end
    end)
    local cordis = require("cordis")
    ctx:plugin({
      name = "async-child-svc",
      apply = cordis.async(function()
        cordis.tick(ctx.host)
        error("ASYNC-CHILD-BOOM")
      end),
    })
  end,
}"#,
            );
            install_probe_events_plugin(&env, "probe", &[("undecl", "emit")]);
            let h = host(&env).await;
            let active = plugin_active(&h.kernel, "undecl");
            let async_active = plugin_active(&h.kernel, "async-child");
            let undecl_cleaned: bool = eval(&h, "return mare.g.undecl_cleaned == true").await;
            let async_cleaned: bool = eval(&h, "return mare.g.async_cleaned == true").await;
            let emitted: Option<String> = eval(&h, r#"return C.cordis:emit("probe/undecl")"#).await;
            let p = problems(&h);
            h.shutdown().await;
            assert!(!active, "undeclared ctx:plugin(S) must fail its owning plugin row: {p}");
            assert!(!async_active, "async-failing ctx:plugin(S) must fail its owning plugin row: {p}");
            assert!(undecl_cleaned, "effect before failing ctx:plugin(S) must roll back immediately: {p}");
            assert!(async_cleaned, "effect before async-failing ctx:plugin(S) must be awaited and rolled back: {p}");
            assert_eq!(emitted, None, "listener before failing ctx:plugin(S) must be disposed on rollback: {p}");
            assert!(p.contains("undecl") && p.contains("does not declare it"), "{p}");
            assert!(p.contains("ASYNC-CHILD-BOOM"), "{p}");
        }

        // 2. Valid ctx:plugin(S) edited to fail in S:constructor rolls back on reload.
        let env2 = TestEnv::new().await;
        let path = write_plugin(
            &env2,
            "extra-plug",
            r#"return {
  name = "extra-plug",
  provide = { "extra" },
  contracts = { extra = { methods = { ping = { doc = "ping", expose = "lua", params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "string" } } } } },
  apply = function(ctx)
    local S = require("cordis").class("S", mare.Service)
    function S:constructor(c) S.super.constructor(self, c, "extra") end
    function S:ping() return "v1" end
    ctx:plugin(S)
  end,
}"#,
        );
        let h = host(&env2).await;
        assert_eq!(eval::<String>(&h, r#"return C.cordis:get("extra"):ping()"#).await, "v1");
        std::fs::write(
            &path,
            r#"return {
  name = "extra-plug",
  provide = { "extra" },
  contracts = { extra = { methods = { ping = { doc = "ping", expose = "lua", params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "string" } } } } },
  apply = function(ctx)
    local S = require("cordis").class("S", mare.Service)
    function S:constructor(c) S.super.constructor(self, c, "extra") end
    ctx:plugin(S) -- missing required method `ping`: S:constructor fails
  end,
}"#,
        )
        .unwrap();
        h.reload_all().await.unwrap();
        let active = plugin_active(&h.kernel, "extra-plug");
        let ping = eval::<String>(&h, r#"return C.cordis:get("extra"):ping()"#).await;
        let p = problems(&h);
        h.shutdown().await;
        assert!(active, "rollback keeps extra-plug active: {p}");
        assert_eq!(ping, "v1", "previous definition stays serving: {p}");
        assert!(p.contains("extra-plug") && p.contains("requires method `ping`"), "{p}");
    })
    .await
}

#[tokio::test]
async fn a_delayed_ctx_plugin_service_settles_at_startup_and_propagates_failure() {
    local(async {
        {
            let env = TestEnv::new().await;
            write_plugin(
                &env,
                "a-late",
                r#"return {
  name = "a-late",
  provide = { "late" },
  contracts = { late = { methods = { ping = { doc = "ping", expose = "lua", params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "string" } } } } },
  apply = function(ctx)
    local S = require("cordis").class("S", mare.Service)
    S.inject = { "dep" }
    function S:constructor(c) S.super.constructor(self, c, "late") end
    function S:ping() return "late-ok" end
    ctx:plugin(S)
  end,
}"#,
            );
            write_plugin(
                &env,
                "z-dep",
                r#"return {
  name = "z-dep",
  provide = { "dep" },
  contracts = { dep = { methods = {} } },
  apply = function(ctx)
    require("mare_lib").service(ctx, "dep", {})
  end,
}"#,
            );
            let h = host(&env).await;
            let has_late = h.service_table("late").is_some();
            let p = problems(&h);
            h.shutdown().await;
            assert!(has_late, "delayed child service must settle before startup finishes: {p}");
        }

        // Failure case: delayed child fails once `dep` arrives -> owning row is reported failed.
        let env2 = TestEnv::new().await;
        write_plugin(
            &env2,
            "a-late",
            r#"return {
  name = "a-late",
  provide = { "late" },
  contracts = { late = { methods = { ping = { doc = "ping", expose = "lua", params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "string" } } } } },
  apply = function(ctx)
    local S = require("cordis").class("S", mare.Service)
    S.inject = { "dep" }
    function S:constructor(c)
      error("DELAYED-BOOM")
    end
    ctx:plugin(S)
  end,
}"#,
        );
        write_plugin(
            &env2,
            "z-dep",
            r#"return {
  name = "z-dep",
  provide = { "dep" },
  contracts = { dep = { methods = {} } },
  apply = function(ctx)
    require("mare_lib").service(ctx, "dep", {})
  end,
}"#,
        );
        let h = host(&env2).await;
        let active = plugin_active(&h.kernel, "a-late");
        let p = problems(&h);
        h.shutdown().await;
        assert!(!active, "a delayed child failure must mark owning plugin inactive: {p}");
        assert!(p.contains("a-late") && p.contains("DELAYED-BOOM"), "{p}");
    })
    .await
}

#[tokio::test]
async fn an_essential_plugins_failing_inject_follower_does_not_block_requests() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }\n").unwrap();
        write_plugin(
            &env,
            "ess-follow",
            r#"return {
  name = "ess-follow",
  required = true,
  apply = function(ctx)
    mare.g.ess_ran = true
    -- immediate follower (session is already up) and delayed follower (late-dep comes later)
    ctx:inject({ "session" }, function()
      error("IMMEDIATE-FOLLOWER-BOOM")
    end)
    ctx:inject({ "late-dep" }, function()
      error("DELAYED-FOLLOWER-BOOM")
    end)
  end,
}"#,
        );
        write_plugin(
            &env,
            "z-late-dep",
            r#"return {
  name = "z-late-dep",
  provide = { "late-dep" },
  contracts = { ["late-dep"] = { methods = {} } },
  apply = function(ctx)
    require("mare_lib").service(ctx, "late-dep", {})
  end,
}"#,
        );
        let opts = env.host().user_config(false).no_session().opts;
        let h = mare_host::Host::new(opts).await.unwrap();
        let active = plugin_active(&h.kernel, "ess-follow");
        let ran: bool = eval(&h, "return mare.g.ess_ran == true").await;
        let warnings = h.plugin_warnings();
        let prompt = h.prompt("hello", serde_json::json!({})).await.unwrap();
        h.shutdown().await;
        assert!(ran, "essential plugin apply must have executed");
        assert!(active, "essential plugin with failing inject followers must stay ACTIVE; warnings={warnings:?}");
        assert!(warnings.is_empty(), "failing inject follower must not raise plugin setup failure: {warnings:?}");
        assert!(prompt["error"].is_null(), "requests must not be blocked: {prompt}");
    })
    .await
}

#[tokio::test]
async fn failed_plugin_reload_and_repeated_failures_inspection_stay_bounded_in_time() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }\n").unwrap();
        let path = write_plugin(
            &env,
            "opt-flaky",
            "-- rev 0\nreturn { name = 'opt-flaky', apply = function() if mare.g.break_opt then error('opt-boom') end end }",
        );
        let opts = env.host().user_config(false).no_session().opts;
        let h = mare_host::Host::new(opts).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            exec(&h, "mare.g.break_opt = true").await;
            std::fs::write(
                &path,
                "-- rev 1\nreturn { name = 'opt-flaky', apply = function() if mare.g.break_opt then error('opt-boom') end end }",
            )
            .unwrap();
            h.reload_all().await.unwrap();
            let count: usize = eval(
                &h,
                "local n = 0; for _ = 1, 25 do n = #mare.plugins.failures() end; return n",
            )
            .await;
            assert_eq!(count, 1);
            let prompt = h.prompt("hello", serde_json::json!({})).await.unwrap();
            assert!(prompt["error"].is_null(), "prompt should succeed with optional plugin failure: {prompt}");
        })
        .await
        .expect("reload + repeated failures()/dump() + prompt must complete within timeout");
        h.shutdown().await;
    })
    .await
}
