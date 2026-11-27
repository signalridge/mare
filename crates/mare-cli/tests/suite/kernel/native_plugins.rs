//! A plugin definition is a Cordis component: its `apply` gets the cordis ctx itself, while the
//! host wiring (owner, module dir, active set) stays.

use super::{host, write_plugin};
use crate::common::*;

/// A native provider: a mare.Service whose `add` is an effect of the caller's fiber.
const COUNTER: &str = r#"return { name = "counter", provide = { "counter" },
  config_schema = { greeting = { type = "string", default = "hi" } },
  contracts = { counter = { methods = {
    add = { doc = "add an item owned by the caller", expose = "lua",
      params = { type = "object", properties = { item = { type = "string" } }, required = { "item" },
        additionalProperties = false }, order = { "item" }, result = { type = "object" } },
    list = { doc = "items", expose = "lua",
      params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "array", items = { type = "string" } } },
  } } },
  apply = function(ctx, config)
    local cordis = require("cordis")
    -- the compat layer would hand an old-style table with `on_dispose`; a native ctx has none
    local has_old = pcall(function() return ctx.on_dispose end)
    mare._counter_native = not has_old and type(ctx.effect) == "function"
    mare._counter_greeting = config.greeting
    local items = {}
    local Counter = cordis.class("Counter", mare.Service)
    function Counter:constructor(c)
      Counter.super.constructor(self, c, "counter")
    end
    function Counter:add(item)
      self.ctx:effect(function()
        items[item] = true
        return function() items[item] = nil end
      end, "counter item")
    end
    function Counter:list()
      local out = {}
      for k in pairs(items) do out[#out + 1] = k end
      table.sort(out)
      return out
    end
    Counter.new(ctx)
  end }"#;

/// A native consumer: declares `inject`, registers through the service, cleans up with an effect.
const USER: &str = r#"return { name = "user", inject = { "counter" },
  apply = function(ctx)
    ctx.counter:add("from-user")
    ctx:effect(function() return function() mare._user_cleaned = (mare._user_cleaned or 0) + 1 end end, "user cleanup")
  end }"#;

#[tokio::test]
async fn a_native_definition_gets_the_cordis_ctx_and_its_registrations_are_its_own_effects() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "counter", COUNTER);
        write_plugin(&env, "user", USER);
        let h = host(&env).await;
        let problems = h.kernel.problems().join("\n");
        let (native, greeting): (Option<bool>, Option<String>) =
            eval(&h, "return mare._counter_native, mare._counter_greeting").await;
        let (native, greeting) = (native.unwrap_or(false), greeting.unwrap_or_default());
        assert!(native, "apply got the compat layer, not the cordis ctx: {problems}");
        assert_eq!(greeting, "hi", "config reaches a native apply");
        let svc = h
            .service_table("counter")
            .unwrap_or_else(|| panic!("the native provider's service is not up: {}", h.kernel.problems().join("\n")));
        global(&h, "COUNTER", svc);
        let items: Vec<String> = eval(&h, "return COUNTER:list()").await;
        assert_eq!(items, vec!["from-user".to_string()], "the native consumer registered through inject");

        // only the consumer goes away (a rows-driven unload: the provider keeps running): what it
        // registered goes with its fiber, and its own cleanup runs exactly once
        unload_plugin(&h.kernel, "user").await;
        let (items, cleaned): (Vec<String>, Option<i64>) = eval(&h, "return COUNTER:list(), mare._user_cleaned").await;
        let provider_up = plugin_active(&h.kernel, "counter") && !plugin_active(&h.kernel, "user");
        h.shutdown().await;
        assert!(provider_up, "the provider must stay up while only the consumer is unloaded");
        assert!(items.is_empty(), "the consumer's registration outlived its fiber: {items:?}");
        assert_eq!(cleaned, Some(1), "the consumer's effect cleanup ran exactly once");
    })
    .await
}

/// An optional service, the upstream way (Cordis has no optional inject: every `inject` name is
/// required, packages/core/src/registry.ts:11): the work that needs it runs in a follower fiber,
/// `ctx:inject({ name }, fn)`, which starts when the service appears and is cleaned up with it.
const WATCHER: &str = r#"return { name = "watcher",
  apply = function(ctx)
    mare._watcher_applied = true
    ctx:inject({ "counter" }, function(sub)
      mare._follower_runs = (mare._follower_runs or 0) + 1
      sub.counter:add("from-follower")
    end)
  end }"#;

#[tokio::test]
async fn a_native_follower_fiber_runs_when_an_optional_service_appears() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "watcher", WATCHER);
        let h = host(&env).await;
        let (applied, runs): (Option<bool>, Option<i64>) =
            eval(&h, "return mare._watcher_applied, mare._follower_runs").await;
        assert_eq!(
            (applied, runs),
            (Some(true), None),
            "the plugin starts without the service; the follower waits: {}",
            h.kernel.problems().join("\n")
        );

        let counter = write_plugin(&env, "counter", COUNTER);
        h.reload_all().await.unwrap();
        let svc = h.service_table("counter").unwrap_or_else(|| panic!("{}", h.kernel.problems().join("\n")));
        global(&h, "COUNTER", svc);
        let (runs, items): (Option<i64>, Vec<String>) = eval(&h, "return mare._follower_runs, COUNTER:list()").await;
        assert_eq!(runs, Some(1), "the follower runs once the service appears");
        assert_eq!(items, vec!["from-follower".to_string()]);

        // the service goes: the follower is cleaned up, the plugin itself stays
        std::fs::remove_file(&counter).unwrap();
        h.reload_all().await.unwrap();
        let gone = h.service_table("counter").is_none();
        h.shutdown().await;
        assert!(gone, "the provider was removed");
    })
    .await
}

/// A native listener (`ctx:on`) hears an emit made on the cordis side. Until CK-E, events still
/// dispatch through the Rust engine, so a native listener hears only cordis-side emits (an
/// `ctx:emit` of a legacy plugin or the host does not reach it); this pins the cordis path only.
const EARS: &str = r#"return { name = "ears",
  contracts = { probe = { owner = "ears", events = { native = { doc = "probe", mode = "emit", expose = "process" } } } },
  apply = function(ctx)
    ctx:on("probe/native", function(v) mare._heard = v end)
    mare._emit_native = function(v) ctx:emit("probe/native", v) end
  end }"#;

#[tokio::test]
async fn a_native_listener_hears_a_cordis_side_emit() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "ears", EARS);
        let h = host(&env).await;
        let heard: Option<String> = eval(&h, "mare._emit_native('hello'); return mare._heard").await;
        h.shutdown().await;
        assert_eq!(heard.as_deref(), Some("hello"), "{}", "the native listener did not hear the cordis emit");
    })
    .await
}

/// A native registration method returns what `self.ctx:effect` returns (a callable
/// EffectDisposer, not a function): the contract guard accepts it, and the release the caller gets
/// runs the effect's cleanup once, waiting for it when it yields.
const REGISTRAR: &str = r#"return { name = "registrar", provide = { "registrar" },
  contracts = { registrar = { methods = {
    add = { doc = "register an item", expose = "both", registration = true },
  } } },
  apply = function(ctx)
    local cordis = require("cordis")
    mare._cleanups = 0
    local Registrar = cordis.class("Registrar", mare.Service)
    function Registrar:constructor(c)
      Registrar.super.constructor(self, c, "registrar")
    end
    function Registrar:add()
      return self.ctx:effect(function()
        -- a cleanup that waits (I/O): the release must let it yield
        return function() mare.task.sleep(5) mare._cleanups = mare._cleanups + 1 end
      end, "registrar item")
    end
    Registrar.new(ctx)
  end }"#;

#[tokio::test]
async fn a_native_registration_method_may_return_the_effect_disposer() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "registrar", REGISTRAR);
        let h = host(&env).await;
        let svc = h
            .service_table("registrar")
            .unwrap_or_else(|| panic!("the native provider is not up: {}", h.kernel.problems().join("\n")));
        global(&h, "REGISTRAR", svc);
        let (ok, kind): (bool, String) = eval(
            &h,
            "local ok, release = pcall(REGISTRAR.add, REGISTRAR) \
             if ok then REL = release end \
             return ok, ok and type(release) or tostring(release and release.code or release)",
        )
        .await;
        assert!(ok, "the disposer was refused: {kind}");
        assert_eq!(kind, "function", "the release handed on is a function");
        exec(&h, "REL() REL()").await;
        let cleanups: i64 = eval(&h, "return mare._cleanups").await;
        h.shutdown().await;
        assert_eq!(cleanups, 1, "the effect's cleanup runs once");
    })
    .await
}

/// The same through the JSON registration path: the kernel holds the disposer by id and the
/// release runs the effect's cleanup once.
#[tokio::test]
async fn a_native_effect_disposer_is_held_and_released_once_for_a_json_caller() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "registrar", REGISTRAR);
        let h = host(&env).await;
        // the JSON caller is an owner record (no Ctx userdata), calling through the one
        // Lua-side entry to the export surface
        let caller_id = h.kernel.open_owner("<root>", "root").unwrap();
        let args = h.lua().create_table().unwrap();
        let (ok, value) =
            h.kernel.call_export_from_lua(caller_id, "registrar", "add", mlua::Value::Table(args), None).await.unwrap();
        assert!(ok, "the call failed: {value:?}");
        let id: String = value.as_table().unwrap().get("registration_id").unwrap();
        assert!(h.kernel.release_registration(caller_id, &id).await.unwrap(), "released");
        assert!(!h.kernel.release_registration(caller_id, &id).await.unwrap(), "not released twice");
        let cleanups: i64 = eval(&h, "return mare._cleanups").await;
        h.shutdown().await;
        assert_eq!(cleanups, 1, "the effect's cleanup runs once");
    })
    .await
}

/// a mare.Service class whose contract requires a method named `new`
/// (session's does) is still constructed as a component. cordis builds components through
/// class.construct, not `Class.new`, which the method shadows (cordis-lua v0.2.9, deviation D4).
const NEW_METHOD: &str = r#"return { name = "newsvc", provide = { "newsvc" },
  contracts = { newsvc = { methods = {
    new = { doc = "start a new one", expose = "lua", params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "string" } },
  } } },
  apply = function(ctx)
    local Svc = require("cordis").class("NewSvc", mare.Service)
    function Svc:constructor(c)
      Svc.super.constructor(self, c, "newsvc")
      mare._newsvc_built = (mare._newsvc_built or 0) + 1
    end
    function Svc:new() return "fresh" end
    ctx:plugin(Svc)
  end }"#;

#[tokio::test]
async fn a_service_with_a_method_named_new_is_still_constructed() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "newsvc", NEW_METHOD);
        let h = host(&env).await;
        let svc = h
            .service_table("newsvc")
            .unwrap_or_else(|| panic!("the provider is not up: {}", h.kernel.problems().join("\n")));
        global(&h, "NEWSVC", svc);
        let (built, answer): (i64, String) = eval(&h, "return mare._newsvc_built, NEWSVC:new()").await;
        h.shutdown().await;
        assert_eq!((built, answer.as_str()), (1, "fresh"));
    })
    .await
}
