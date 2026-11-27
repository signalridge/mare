//! Plugin kernel lifecycle: everything registered through a ctx is reclaimed on dispose,
//! services shadow and un-shadow, reload re-applies dependents, cycles are rejected.

use mare_kernel::{Kernel, Origin, PluginSource};
use mare_plugin::{self as lua, Env, Mode};
use mare_shared::ui::UiShared;

mod close_and_reload;
#[path = "kernel_contracts.rs"]
mod contracts;
mod cordis_contexts;
mod events_and_timers;
mod flip_diagnostics;
mod follow;
mod host_methods;
mod lifecycle;
mod lifecycle_stalls;
mod lua_params;
mod native_bus;
mod native_plugins;
mod native_tasks;
mod one_bus;
mod policy_service;
mod ports;
mod raw_provide;
mod reload_snapshot;
mod semantics;
mod services;
mod taps;

const REGISTRY: &str = r#"
return {
  name = "registry",
  provide = { "test-registry" },
  apply = function(ctx)
    local items = {}
    local Registry = require("cordis").class("Registry", mare.Service)
    function Registry:constructor(c)
      Registry.super.constructor(self, c, "test-registry", { contract = { methods = {
        -- a plugin's service declares what other plugins may call (contracts); `registry`
        -- itself is a built-in contract name
        add = { doc = "add", expose = "lua", params = { type = "object", properties = { name = { type = "string" } }, additionalProperties = false }, order = { "name" }, result = { type = "object" } },
        count = { doc = "count", expose = "lua", params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "integer" } },
      } } })
    end
    -- an item is an effect of the caller (self.ctx): it goes when the caller does
    function Registry:add(name)
      self.ctx:effect(function()
        items[name] = true
        return function() items[name] = nil end
      end, "test-registry item")
    end
    function Registry:count()
      local n = 0
      for _ in pairs(items) do n = n + 1 end
      return n
    end
    ctx:plugin(Registry)
  end,
}
"#;

const CONSUMER: &str = r#"
return {
  name = "consumer",
  inject = { "test-registry", "task" },
  apply = function(ctx)
    ctx["test-registry"]:add("a")
    ctx["test-registry"]:add("b")
    ctx:on("ping", function(n) mare._pings = (mare._pings or 0) + n end)
    _G.leak = true -- stays in this plugin's env
    mare._std_patch = pcall(function() string.evil = 1 end)
    ctx:on("ask", function() return "consumer" end)
    ctx:on("rewrite", function(v) return v .. "+c" end)
    ctx:spawn(function() mare.task.sleep(1e9) end)
    ctx:effect(function() return function() mare._disposed = (mare._disposed or 0) + 1 end end)
  end,
}
"#;

fn src(name: &str, code: &'static str) -> PluginSource {
    PluginSource { name_hint: name.into(), origin: Origin::User, path: None, code: Some(code.as_bytes()) }
}

fn src_at(name: &str, origin: Origin, code: &'static str) -> PluginSource {
    PluginSource { name_hint: name.into(), origin, path: None, code: Some(code.as_bytes()) }
}

/// Write a user plugin file (`config/plugins/<name>.lua`) into the test home; returns its path.
pub(super) fn write_plugin(env: &crate::common::TestEnv, name: &str, src: &str) -> std::path::PathBuf {
    let dir = env.home.path().join("config/plugins");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.lua"));
    std::fs::write(&path, src).unwrap();
    path
}

/// A host with the user plugins and no session, whatever problems they cause (tests read them).
pub(super) async fn host(env: &crate::common::TestEnv) -> std::sync::Arc<mare_host::Host> {
    env.host().user_plugins().no_session().build_unchecked_exposed().await
}

fn kernel() -> Kernel {
    let l = lua::new_lua().unwrap();
    let env = Env {
        cwd: std::env::current_dir().unwrap(),
        mode: Mode::Child,
        ui: UiShared::new(),
        config: Default::default(),
        args: serde_json::Value::Null,
    };
    lua::install(&l, &env).unwrap();
    let k = Kernel::new(l, Default::default()).unwrap();
    let hooks_contract: mlua::Value =
        k.lua().load(lua::service_contracts_lua().as_ref()).eval::<mlua::Table>().unwrap().get("hooks").unwrap();
    let contracts: mlua::Table = k
        .lua()
        .load(
            r#"return {
      registry = { methods = { add = { doc = "add", lua_unchecked = true, result = {} }, count = { doc = "count", lua_unchecked = true, result = {} } } },
      svc = { methods = { run = { doc = "run", optional = true, lua_unchecked = true, result = {} }, call = { doc = "call", optional = true, lua_unchecked = true, result = {} } } },
      base = { methods = {} }, x = { methods = {} }, y = { methods = {} }, rapid = { methods = {} },
    }"#,
        )
        .eval()
        .unwrap();
    contracts.set("hooks", hooks_contract).unwrap();
    k.install_service_contracts(contracts).unwrap();
    k
}
