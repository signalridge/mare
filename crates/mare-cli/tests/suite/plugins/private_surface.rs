//! Exhaustive traversal of the public Lua surface exposed to plugins.
//!
//! Confirms that `mare`, `ui`, every registered public service on `ctx`, every `---@meta`
//! public module, and `package.{preload,loaded}` expose zero single-underscore private fields
//! both at startup and after `/reload`.

use std::collections::BTreeSet;
use std::path::Path;

use mare_shared::ui::Mode;
use serde_json::{Value, json};

use crate::common::*;

const WALK_PUBLIC_SURFACE_LUA: &str = r#"
local function is_private_key(k)
  return type(k) == "string" and k:sub(1, 1) == "_" and k:sub(1, 2) ~= "__"
end

local function walk_table(root, label, out, opts)
  opts = opts or {}
  local skip_paths = opts.skip_paths or {}
  local seen = {}
  local function visit(tbl, path)
    if type(tbl) ~= "table" or seen[tbl] then
      return
    end
    seen[tbl] = true
    if skip_paths[path] then
      return
    end
    for k, v in next, tbl do
      local key_str = tostring(k)
      local child = path .. "." .. key_str
      if is_private_key(k) then
        out[#out + 1] = child
      elseif type(v) == "table" and not skip_paths[child] then
        visit(v, child)
      end
    end
    local mt = getmetatable(tbl)
    if type(mt) == "table" and type(mt.__index) == "table" and not seen[mt.__index] then
      visit(mt.__index, path .. "<__index>")
    end
  end
  visit(root, label)
end

local function check_surface(public_modules, services)
  local leaks = {}

  -- 0. Global `_G` top-level keys: no single-underscore private globals
  -- (standard Lua defines `_G` and `_VERSION`).
  local std_globals = { _G = true, _VERSION = true }
  for k in next, _G do
    if is_private_key(k) and not std_globals[k] then
      leaks[#leaks + 1] = "_G." .. tostring(k)
    end
  end

  -- 1. Global `mare` table and nested subtables (`mare.g` holds arbitrary plugin state).
  walk_table(mare, "mare", leaks, { skip_paths = { ["mare.g"] = true } })

  -- 2. Global `ui` table and nested subtables.
  if type(ui) == "table" then
    walk_table(ui, "ui", leaks)
  end

  -- 3. Every registered public service exposed on `ctx`.
  for name, svc in next, services do
    if type(svc) == "table" then
      walk_table(svc, "ctx." .. tostring(name), leaks)
    end
  end

  -- 4. Every `---@meta` public module loaded through `require`.
  -- Vendored upstream `cordis` defines internal `_`-prefixed class methods on its own
  -- prototypes (guarded statically against external access by the Lua public API lint),
  -- so for `cordis` we check top-level exports only; all mare modules are walked deeply.
  for _, mod_name in ipairs(public_modules) do
    local ok, mod = pcall(require, mod_name)
    if ok and type(mod) == "table" then
      if mod_name == "cordis" then
        for k in next, mod do
          if is_private_key(k) then
            leaks[#leaks + 1] = "require(cordis)." .. tostring(k)
          end
        end
      else
        walk_table(mod, "require(" .. mod_name .. ")", leaks)
      end
    end
  end

  -- 5. `package.preload` and `package.loaded`: no `_`-prefixed entries (except Lua's
  -- standard `package.loaded._G`) or internal staging leftovers.
  local forbidden_preload = {
    config_api_ui_layout = true,
    config_api_validate = true,
  }
  local forbidden_loaded = {
    ["mare.plugin_slots"] = true,
    ["mare.preset_cleared"] = true,
    ["mare.plugin_preset_entry"] = true,
  }
  if type(package) == "table" then
    if type(package.preload) == "table" then
      for k in next, package.preload do
        if is_private_key(k) or forbidden_preload[k] then
          leaks[#leaks + 1] = "package.preload." .. tostring(k)
        end
      end
    end
    if type(package.loaded) == "table" then
      for k in next, package.loaded do
        if (is_private_key(k) and k ~= "_G") or forbidden_loaded[k] then
          leaks[#leaks + 1] = "package.loaded." .. tostring(k)
        end
      end
    end
  end

  table.sort(leaks)
  return leaks
end

return {
  walk_table = walk_table,
  check_surface = check_surface,
}
"#;

fn scan_public_meta_modules() -> Vec<String> {
    fn walk(dir: &Path, out: &mut BTreeSet<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|s| s.to_str()) == Some("lua") {
                let text = std::fs::read_to_string(&p).unwrap();
                for line in text.lines() {
                    if let Some(rest) = line.strip_prefix("---@meta")
                        && rest.chars().next().is_some_and(char::is_whitespace)
                    {
                        let trimmed = rest.trim_start();
                        let len = trimmed
                            .bytes()
                            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
                            .count();
                        if len > 0 {
                            let name = &trimmed[..len];
                            if name != "_" {
                                out.insert(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    let types_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mare-plugin/preset/types");
    let mut out = BTreeSet::new();
    walk(&types_dir, &mut out);
    out.into_iter().collect()
}

fn scan_declared_service_names() -> Vec<String> {
    let types_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mare-plugin/preset/types/mare/services");
    let mut out = BTreeSet::new();

    // 1. Parse all `---@overload fun(self: mare.Ctx, name: "<svc>")` entries in `ctx.lua`.
    let ctx_lua = std::fs::read_to_string(types_dir.join("ctx.lua")).unwrap();
    for line in ctx_lua.lines() {
        if let Some(after) = line.split("name: \"").nth(1)
            && let Some(name) = after.split('"').next()
            && !name.is_empty()
        {
            out.insert(name.to_string());
        }
    }

    // 2. Union with contract module filenames (with `_` -> `-` fallback for e.g. `update_check`).
    if let Ok(rd) = std::fs::read_dir(types_dir.join("contracts")) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("lua")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && !matches!(stem, "schema" | "events")
            {
                out.insert(stem.to_string());
                if stem.contains('_') {
                    out.insert(stem.replace('_', "-"));
                }
            }
        }
    }

    // 3. Include host utility services that do not declare a separate contract file.
    out.insert("task".to_string());

    out.into_iter().collect()
}

async fn collect_surface_leaks(h: &mare_host::Host, public_modules: &[String]) -> Vec<String> {
    let declared = scan_declared_service_names();
    let lua = h.lua();

    let services_tbl = lua.create_table().unwrap();
    let mut registered = Vec::new();
    for name in &declared {
        if let Some(tbl) = h.service_table(name) {
            services_tbl.set(name.clone(), tbl).unwrap();
            registered.push(name.clone());
        }
    }

    for required in [
        "kernel",
        "bus",
        "policy",
        "options",
        "agent",
        "llm",
        "tools",
        "permissions",
        "approvals",
        "session",
        "ui",
        "commands",
        "status",
    ] {
        assert!(
            registered.iter().any(|s| s == required),
            "expected core service `{required}` to be registered; saw {registered:?}"
        );
    }

    lua.globals().set("_MARE_TEST_SERVICES", services_tbl).unwrap();
    let mods_json = serde_json::to_string(public_modules).unwrap();

    let code = format!(
        r#"
local services = _MARE_TEST_SERVICES
_MARE_TEST_SERVICES = nil
local walker = (function()
{WALK_PUBLIC_SURFACE_LUA}
end)()
local mods = mare.json.decode({mods_json:?})
return mare.json.encode(walker.check_surface(mods, services))
"#
    );
    let raw: String = eval(h, &code).await;
    let parsed: Value = serde_json::from_str(&raw).unwrap();
    match parsed {
        Value::Array(arr) => arr.into_iter().map(|v| v.as_str().unwrap().to_string()).collect(),
        Value::Object(map) if map.is_empty() => Vec::new(),
        other => panic!("unexpected surface check JSON: {other}"),
    }
}

#[tokio::test]
async fn public_surface_walker_detects_nested_and_metatable_private_fields() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let code = format!(
            r#"
local walker = (function()
{WALK_PUBLIC_SURFACE_LUA}
end)()
local proto = {{ _proto_leak = 1, ok = true }}
local sample = setmetatable({{
  nested = {{ _nested_leak = true, __metamethod = true }},
  clean = 42,
}}, {{ __index = proto }})
local leaks = {{}}
walker.walk_table(sample, "sample", leaks)
table.sort(leaks)
return mare.json.encode(leaks)
"#
        );
        let raw: String = eval(&h, &code).await;
        h.shutdown().await;
        let leaks: Vec<String> = serde_json::from_str(&raw).unwrap();
        assert_eq!(leaks, vec!["sample.nested._nested_leak", "sample<__index>._proto_leak"]);
    })
    .await;
}

#[tokio::test]
async fn public_lua_surface_has_zero_private_fields_across_startup_and_reload() {
    local(async {
        let env = TestEnv::new().await;
        let public_modules = scan_public_meta_modules();
        assert!(!public_modules.is_empty(), "expected ---@meta public modules in preset/types");

        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo', border = 'single' }\n")
            .unwrap();

        let mut b = env.host().user_config(true).no_session().config(json!({
            "plugins": { "onboarding": { "plugins": false } }
        }));
        b.opts.mode = Mode::Tui;
        let h = b.build().await;

        let startup_leaks = collect_surface_leaks(&h, &public_modules).await;
        assert!(
            startup_leaks.is_empty(),
            "public Lua surface leaked private fields at startup:\n{}",
            startup_leaks.join("\n")
        );

        let report = h.reload_config().await.expect("config reload must succeed");
        let failures = report["failures"].as_array().cloned().unwrap_or_default();
        assert!(failures.is_empty(), "reload failures: {failures:?}");

        let reload_leaks = collect_surface_leaks(&h, &public_modules).await;
        assert!(
            reload_leaks.is_empty(),
            "public Lua surface leaked private fields after /reload:\n{}",
            reload_leaks.join("\n")
        );

        h.shutdown().await;
    })
    .await;
}
