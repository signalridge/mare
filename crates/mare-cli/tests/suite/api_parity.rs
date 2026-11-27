//! API parity, run by `just accept`: the declared surfaces and the running
//! one agree. Contracts ↔ `mare rpc schema` ↔ the runtime catalog; no RPC domain method outside a
//! contract except the explicit lists below; every contract has a built-in provider; contract
//! methods ↔ LuaLS service types; hand-written primitive types ↔ the functions that exist.
//! The exemption lists only shrink: each entry names why it exists and what removes it.
use crate::common::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Like reference implementation profile reconciliation test, preserve explicit settings and repeated selections:
/// reference implementation/packages/boot/app-boot/tests/profile-plugins.spec.ts:86-116.
#[tokio::test]
async fn minimal_test_profile_excludes_non_minimal_builtins_until_enabled() {
    local(async {
        let env = TestEnv::new().await;
        let minimal = env.host().no_session().build().await;
        assert!(minimal.service_table("compact").is_none(), "minimal profile must omit compact");
        assert!(minimal.service_table("plugins").is_none(), "minimal profile must omit pkg");
        minimal.shutdown().await;

        let selective = env
            .host()
            .config(json!({ "plugins": { "compact": { "enabled": false, "auto": false } } }))
            .builtins(&["compact"])
            .builtins(&[])
            .builtins(&["compact"]);
        assert_eq!(selective.opts.config["plugins"]["compact"], json!({ "enabled": true, "auto": false }));
        let selective = selective.no_session().build().await;
        assert!(selective.service_table("compact").is_some(), "opt-in must load compact");
        assert!(selective.service_table("plugins").is_none(), "opt-in must leave pkg disabled");
        selective.shutdown().await;

        let full = env.host().all_builtins().no_session().build().await;
        assert!(full.service_table("compact").is_some(), "all builtins must load compact");
        assert!(full.service_table("plugins").is_some(), "all builtins must load pkg");
        assert!(
            !full.kernel.plugin_status().iter().any(|(name, _, active)| name == "permissions" && *active),
            "all builtins must preserve the explicit permissions disable"
        );
        full.shutdown().await;

        let explicit = env
            .host()
            .builtins(&["compact"])
            .config(json!({ "plugins": { "plugins": { "enabled": false, "watch_ms": 0 } } }))
            .all_builtins()
            .all_builtins();
        assert!(explicit.opts.config["plugins"].get("preset").is_none());
        assert_eq!(explicit.opts.config["plugins"]["compact"]["enabled"], json!(true));
        assert_eq!(explicit.opts.config["plugins"]["plugins"], json!({ "enabled": false, "watch_ms": 0 }));
        assert_eq!(explicit.opts.config["plugins"]["permissions"]["enabled"], json!(false));

        let disabled = env
            .host()
            .builtins(&["compact"])
            .config(json!({ "plugins": { "compact": { "enabled": false } } }))
            .all_builtins()
            .no_session()
            .build()
            .await;
        assert!(disabled.service_table("compact").is_none(), "later explicit false must win");
        assert!(disabled.service_table("plugins").is_some(), "all builtins must still load other plugins");
        disabled.shutdown().await;

        let reset = env.host().builtins(&["compact"]).user_config(false);
        assert_eq!(reset.opts.config, json!({}), "user_config clears previous opt-ins and defaults");
        let after_reset = reset
            .config(json!({ "model": "faux/echo", "plugins": { "preset": "minimal", "permissions": { "enabled": false } } }))
            .builtins(&["compact"])
            .no_session()
            .build()
            .await;
        assert!(after_reset.service_table("compact").is_some(), "opt-in after user_config must work");
        assert!(after_reset.service_table("plugins").is_none(), "the new preset still excludes pkg");
        after_reset.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn test_builtin_selection_rejects_non_object_config_and_keeps_empty_selection() {
    let env = TestEnv::new().await;
    let config = json!({ "plugins": [] });
    assert_eq!(env.host().config(config.clone()).builtins(&[]).opts.config["plugins"], config["plugins"]);
    for (config, all) in [
        (json!({ "plugins": [] }), false),
        (json!({ "plugins": { "compact": false } }), false),
        (json!({ "plugins": [] }), true),
        (json!(null), false),
        (json!(null), true),
    ] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let builder = env.host().config(config);
            if all { builder.all_builtins() } else { builder.builtins(&["compact"]) }
        }));
        assert!(result.is_err(), "non-object test plugin config must panic");
    }
}

fn preset() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../mare-plugin/preset")
}

fn lua_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            lua_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "lua") {
            out.push(path);
        }
    }
}

fn read_all(dir: &str) -> Vec<(std::path::PathBuf, String)> {
    let mut files = vec![];
    lua_files(&preset().join(dir), &mut files);
    files.into_iter().map(|path| (path.clone(), std::fs::read_to_string(path).unwrap())).collect()
}

/// The built-in contracts, parsed by the kernel's own parser.
fn contracts() -> BTreeMap<String, mare_kernel::ServiceContract> {
    let lua = mlua::Lua::new();
    let value: mlua::Value = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
    mare_kernel::contracts::parse_contracts(&lua, value).unwrap()
}

#[test]
fn theme_preview_contract_accepts_theme_and_rejects_name() {
    let contracts = contracts();
    let preview = &contracts["theme"].methods["preview"];
    let schema = mare_shared::schema::Schema::compile_contract(preview.wire_params()).unwrap();
    assert!(schema.validate(&json!({ "theme": "dark" })).is_ok());
    assert!(schema.validate(&json!({ "name": "dark" })).is_err());
    assert!(schema.validate(&json!({ "theme": "dark", "name": "dark" })).is_err());
}

#[test]
fn session_pages_and_items_use_the_single_contract_shape() {
    let contracts = contracts();
    let methods = &contracts["session"].methods;
    for name in ["list", "list_items", "list_records", "search", "search_items"] {
        let params = mare_shared::schema::Schema::compile_contract(methods[name].params.clone()).unwrap();
        let result = mare_shared::schema::Schema::compile_contract_value(methods[name].result.clone()).unwrap();
        let mut args = json!({ "cursor": "opaque", "limit": 1 });
        if name == "list_records" {
            args["kind"] = json!("note");
        }
        if name == "search_items" {
            args["session_id"] = json!("s1");
        }
        assert!(params.validate(&args).is_ok(), "{name}");
        args["limit"] = json!(0);
        assert!(params.validate(&args).is_err(), "{name} accepts limit 0");
        let page = if name.starts_with("search") {
            json!({ "data": [], "corpus_revision": "g", "incomplete": false })
        } else {
            json!({ "data": [] })
        };
        assert!(result.validate(&page).is_ok(), "{name}");
        let mut old = page.clone();
        old["hits"] = json!([]);
        assert!(result.validate(&old).is_err(), "{name} accepts old hits");
        old = page;
        old["cursor"] = json!("old");
        assert!(result.validate(&old).is_err(), "{name} accepts old output cursor");
    }
    let get = mare_shared::schema::Schema::compile_contract(methods["get_item"].params.clone()).unwrap();
    assert!(get.validate(&json!({ "session_id": "s1", "item_id": "i1" })).is_ok());
    assert!(get.validate(&json!({ "item_id": "i1" })).is_err());
    assert!(get.validate(&json!({ "session_id": "s1", "event_id": "e1" })).is_err());
    let record = mare_shared::schema::Schema::compile_contract_value(methods["add_record"].result.clone()).unwrap();
    assert!(record.validate(&json!({ "record_id": "r1", "durable": true })).is_ok());
    assert!(record.validate(&json!({ "item_id": "r1", "durable": true })).is_err());
}

fn rpc_schema() -> Value {
    let out = mare_cmd().args(["rpc", "schema"]).run_output();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// The RPC methods derived from contracts, by name.
fn derived() -> BTreeMap<String, Value> {
    let lua = mlua::Lua::new();
    let value: mlua::Value = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
    mare_kernel::contracts::rpc_methods(&lua, value).unwrap()
}

#[tokio::test]
async fn contracts_match_the_rpc_schema_and_the_runtime_catalog() {
    let schema = rpc_schema();
    let methods = schema["methods"].as_object().unwrap();
    for (name, entry) in derived() {
        let listed =
            methods.get(&name).unwrap_or_else(|| panic!("contract method `{name}` is missing from mare rpc schema"));
        assert_eq!(listed["params"], entry["params"], "{name} params");
        assert_eq!(listed["result"], entry["result"], "{name} result");
    }
    // the running catalog (services.list) is the same contract: same methods, same schemas
    local(async {
        let env = TestEnv::new().await;
        let host = env.host().all_builtins().faux(json!([])).build().await;
        let contracts = contracts();
        for service in host.kernel.service_exports() {
            let name = service["name"].as_str().unwrap();
            let Some(contract) = contracts.get(name) else { continue };
            for (method, spec) in service["methods"].as_object().unwrap() {
                let declared = contract
                    .methods
                    .get(method)
                    .unwrap_or_else(|| panic!("{name}.{method} is listed but not in its contract"));
                assert!(declared.remote(), "{name}.{method} is listed but not remote");
                assert_eq!(spec["parameters"], declared.wire_params(), "{name}.{method} parameters");
                assert_eq!(spec["result"], declared.result, "{name}.{method} result");
            }
            for (method, declared) in contract.methods.iter().filter(|(_, spec)| spec.remote() && !spec.optional) {
                assert!(
                    service["methods"].get(method).is_some(),
                    "{name}.{method} is a required remote method but not listed"
                );
                let _ = declared;
            }
        }
        host.shutdown().await;
    })
    .await
}

/// The protocol layer: these stay Rust types in mare-protocol.
const PROTOCOL_LAYER: &[&str] = &[
    "initialize",
    "connection.set_capabilities",
    "shutdown",
    "events.list",
    "events.subscribe",
    "baseline.read",
    "services.list",
    "services.register",
    "registrations.release",
    "peer.info",
    "peer.message",
    "options.get",
    "options.set",
    "config.describe",
    "flags.list",
    "flags.get",
    "state.snapshot",
    "state.compare_exchange",
    // Connection-scoped UI lease/takeover primitives.
    "ui.attach",
    "ui.detach",
];

#[test]
fn rpc_has_no_domain_methods_outside_contracts() {
    let schema = rpc_schema();
    let derived = derived();
    let protocol: BTreeSet<&str> = PROTOCOL_LAYER.iter().copied().collect();
    let methods: BTreeSet<String> = schema["methods"].as_object().unwrap().keys().cloned().collect();
    for method in &methods {
        assert!(
            derived.contains_key(method) || protocol.contains(method.as_str()),
            "`{method}` is an RPC method outside any contract and outside the protocol layer"
        );
    }
    for listed in &protocol {
        assert!(methods.contains(*listed), "`{listed}` is listed but no longer an RPC method: drop it from the list");
    }
    for (service, contract) in contracts().iter().filter(|(_, c)| c.owner.is_none()) {
        for method in &protocol {
            if matches!(*method, "ui.attach" | "ui.detach" | "options.get" | "options.set") {
                continue;
            }
            assert!(
                method.split('.').next() != Some(service.as_str()),
                "`{method}` is hand-written RPC of `{service}`, which has a contract"
            );
        }
        let _ = contract;
    }

    // guards: the generic callback and update are present.
    let callbacks: BTreeSet<&str> = schema["callbacks"].as_object().unwrap().keys().map(String::as_str).collect();
    assert!(callbacks.contains("callback.call"), "callback.call must be in schema callbacks: {callbacks:?}");
    let client_notes: BTreeSet<&str> =
        schema["client_notifications"].as_object().unwrap().keys().map(String::as_str).collect();
    assert!(client_notes.contains("callback.update"), "{client_notes:?}");
    assert!(!schema.to_string().contains("OkResult"), "legacy OkResult leaked into mare rpc schema");
}

/// What the provider check finds in `host`: contracts without a provider, and contract problems
/// (a provider that does not implement its contract is refused and reported).
fn provider_problems(host: &mare_host::Host) -> Vec<String> {
    let mut problems: Vec<String> = contracts()
        .iter()
        .filter(|(_, c)| c.owner.is_none())
        .map(|(name, _)| name)
        .filter(|name| host.kernel.service(name).is_none())
        .map(|name| format!("no built-in provider for `{name}`"))
        .collect();
    problems.extend(host.kernel.problems().into_iter().filter(|p| p.contains("service contract")));
    problems.extend(undeclared_functions(host));
    problems
}

/// Functions a provider's live implementation has but its contract does not
/// declare, looked at after startup: also what a provider added after `provide`.
fn undeclared_functions(host: &mare_host::Host) -> Vec<String> {
    let mut problems = vec![];
    for (name, contract) in contracts().iter().filter(|(_, c)| c.owner.is_none()) {
        let Some(table) = host.service_table(name) else { continue };
        for pair in table.pairs::<mlua::Value, mlua::Value>() {
            let (key, value) = pair.unwrap();
            if let (mlua::Value::String(key), mlua::Value::Function(_)) = (key, value) {
                let key = key.to_string_lossy();
                if !contract.methods.contains_key(&key) {
                    problems.push(format!("`{name}` implementation has `{key}`, which its contract does not declare"));
                }
            }
        }
    }
    problems
}

/// Every contract has a built-in provider that implements it, in a host with every built-in
/// on (the test default turns the permissions plugin off; here it is on).
#[tokio::test]
async fn builtin_providers_implement_every_contract() {
    local(async {
        let env = TestEnv::new().await;
        let host = env
            .host()
            .all_builtins()
            .faux(json!([]))
            .config(json!({ "model": "faux/echo", "plugins": { "permissions": { "enabled": true } } }))
            .build()
            .await;
        let problems = provider_problems(&host);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(host.kernel.service("permissions").is_some(), "the permissions provider was checked");
        host.shutdown().await;
    })
    .await
}

/// Native test plugins that provide services for the tests (the native form of the old
/// the legacy provide of a name, impl and options): one plugin `p-<name>` per service name, written before the host
/// starts. `PROVIDE(name, impl, opts, parent?)` stores the spec (methods `function(self, caller, ...)`
/// as before, `opts.contract`, an optional parent class) in `mare._specs` and re-applies that plugin
/// (admission needs a running owner, which a plugin's apply has); it raises when nothing was
/// published and returns the disposer (drop the spec, re-apply: the plugin's fibers go).
const SVC_LUA: &str = r#"
    function PROVIDE(name, impl, opts, parent)
        mare._specs = mare._specs or {}
        local spec = { impl = impl, opts = opts, parent = parent }
        mare._specs[name] = spec
        local kernel = C.cordis:get("kernel")
        kernel:reload("p-" .. name)
        kernel:reload_barrier()
        if C.cordis:get(name) == nil then
            mare._specs[name] = nil
            error("provide refused: " .. name .. " :: " .. table.concat(kernel:problems(), " ; "), 0)
        end
        return function()
            if mare._specs[name] == spec then
                mare._specs[name] = nil
                kernel:reload("p-" .. name)
                kernel:reload_barrier()
            end
            return true
        end
    end
"#;

fn svc_plugin_source(name: &str) -> String {
    format!(
        r#"return {{ name = "p-{name}", provide = {{ "{name}" }}, apply = function(ctx)
  local spec = (mare._specs or {{}})["{name}"]
  if not spec then return end
  local Class = require("cordis").class("P_{name}", spec.parent or mare.Service)
  function Class:constructor(c)
    Class.super.constructor(self, c, "{name}", spec.opts and {{ contract = spec.opts.contract }} or nil)
  end
  for k, v in pairs(spec.impl) do Class[k] = function(self, ...) return v(self, self.ctx, ...) end end
  ctx:plugin(Class)
end }}"#
    )
}

/// Write the provider plugins for `names`; the host must be built with `user_plugins()`.
fn svc_plugins(env: &TestEnv, names: &[&str]) {
    let dir = env.config_dir().join("plugins");
    std::fs::create_dir_all(&dir).unwrap();
    for name in names {
        std::fs::write(dir.join(format!("p-{name}.lua")), svc_plugin_source(name)).unwrap();
    }
}

/// Counter-test: a provider missing a required contract method makes the same check fail.
#[tokio::test]
async fn a_provider_missing_a_required_method_fails_the_provider_check() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["usage"]);
        // the built-in usage provider is off: one provider per service, so a second one would be
        // refused as already provided before its contract is checked
        let host = env
            .host()
            .user_plugins()
            .faux(json!([]))
            .config(json!({ "plugins": { "usage": { "enabled": false } } }))
            .build()
            .await;
        expose(&host);
        assert!(provider_problems(&host).iter().all(|p| !(p.contains("usage") && p.contains("requires method"))));
        // a usage provider without `get` (required) is refused and reported
        exec(&host, SVC_LUA).await;
        exec(&host, "pcall(PROVIDE, 'usage', { totals = function() end })").await;
        let refused: bool = eval(&host, "return C.cordis:get('usage') == nil").await;
        assert!(refused);
        let problems = provider_problems(&host);
        assert!(
            problems.iter().any(|p| p.contains("usage") && p.contains("requires method")),
            "{problems:?} / {:?}",
            host.kernel.problems()
        );
        host.shutdown().await;
    })
    .await
}

/// Counter-test: under a full contract the implementation table has nothing the contract does
/// not declare. A usage provider with an extra function is refused and reported; a key the
/// contract does not declare is refused through the service view for every caller, not only for
/// work done for a peer.
#[tokio::test]
async fn a_full_contract_refuses_methods_it_does_not_declare() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["empty-svc", "usage"]);
        // the built-in usage provider is off: one provider per service, so a second one would be
        // refused as already provided before its contract is checked
        let host =
            env.host().user_plugins().faux(json!([])).config(json!({ "plugins": { "usage": { "enabled": false } } })).build().await;
        expose(&host);
        exec(&host, SVC_LUA).await;
        let usage_methods = r#"for _, m in ipairs({ "get", "totals", "context_tokens", "set_context", "cost_text", "rate_limits", "quota_lines" }) do"#;
        // a usage provider with exactly the contract's methods is accepted and then checked below
        exec(
            &host,
            &format!(
                r#"local impl = {{}}
                   {usage_methods}
                     impl[m] = function() end
                   end
                   PROVIDE("usage", impl)"#
            ),
        )
        .await;
        let read: String = eval(
            &host,
            r#"local ok, v = pcall(function() return C.cordis:get("usage").cost_of end)
               return (ok and v ~= nil) and "read" or "none""#,
        )
        .await;
        assert_eq!(read, "none", "a key the contract does not declare gives nothing");
        let called: bool =
            eval(&host, r#"return (pcall(function() return C.cordis:get("usage"):cost_of() end))"#).await;
        assert!(!called, "a method the contract does not declare is not callable");
        // a function added to the live implementation after `provide` is found too
        let usage = host.service_table("usage").unwrap();
        usage.raw_set("late", host.kernel.lua().create_function(|_, ()| Ok(())).unwrap()).unwrap();
        let problems = undeclared_functions(&host);
        assert_eq!(problems, ["`usage` implementation has `late`, which its contract does not declare"]);
        host.shutdown().await;
    })
    .await
}

/// The refusing half of the above (extra method, inherited method, empty full contract). A usage provider with an extra function is refused and reported; a key the
/// contract does not declare is refused through the service view for every caller, not only for
/// work done for a peer.
#[tokio::test]
async fn a_full_contract_refuses_a_class_method_it_does_not_declare() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["empty-svc", "usage"]);
        // the built-in usage provider is off: one provider per service, so a second one would be
        // refused as already provided before its contract is checked
        let host =
            env.host().user_plugins().faux(json!([])).config(json!({ "plugins": { "usage": { "enabled": false } } })).build().await;
        expose(&host);
        exec(&host, SVC_LUA).await;
        let usage_methods = r#"for _, m in ipairs({ "get", "totals", "context_tokens", "set_context", "cost_text", "rate_limits", "quota_lines" }) do"#;
        let taken = |host: &mare_host::Host| host.kernel.problems();
        // a refused provider is not published; the reason is a kernel problem
        exec(
            &host,
            &format!(
                r#"local impl = {{}}
                   {usage_methods}
                     impl[m] = function() end
                   end
                   impl.reset = function() end
                   pcall(PROVIDE, "usage", impl)"#
            ),
        )
        .await;
        assert!(!eval::<bool>(&host, r#"return C.cordis:get("usage") ~= nil"#).await, "accepted");
        let refused = taken(&host).join("\n");
        assert!(refused.contains("service `usage` method `reset` is not in its contract"), "{refused}");
        // a method inherited from a parent class is checked too (the native form of a prototype table)
        exec(
            &host,
            &format!(
                r#"local Base = require("cordis").class("UsageBase", mare.Service)
                   {usage_methods}
                     Base[m] = function() end
                   end
                   Base.reset = function() end
                   pcall(PROVIDE, "usage", {{}}, nil, Base)"#
            ),
        )
        .await;
        assert!(!eval::<bool>(&host, r#"return C.cordis:get("usage") ~= nil"#).await, "accepted");
        let prototype = taken(&host).join("\n");
        assert!(prototype.matches("method `reset` is not in its contract").count() >= 2, "{prototype}");
        // a full contract with no methods is still a full contract: nothing else is accepted,
        // and the proxy refuses every key (it was treated as no contract at all)
        let empty: String = eval(
            &host,
            r#"pcall(PROVIDE, "empty-svc", { reset = function() end }, { contract = { methods = {} } })
               local p = C.cordis:get("empty-svc")
               if p then
                 local ok2 = pcall(function() return p:reset() end)
                 return ok2 and "accepted and callable" or "accepted"
               end
               return "refused""#,
        )
        .await;
        assert_eq!(empty, "refused");
        let empty_problem = taken(&host).join("\n");
        assert!(
            empty_problem.lines().any(|l| l.contains("empty-svc") && l.contains("method `reset` is not in its contract")),
            "{empty_problem}"
        );
        let problems = provider_problems(&host);
        assert!(problems.iter().any(|p| p.contains("`reset` is not in its contract")), "{problems:?}");
        host.shutdown().await;
    })
    .await
}

/// `---@class C` directly followed (after its own `---` lines) by `local X = {}`, and the
/// `function X:method(` declared on that local.
fn luals_methods(dir: &str) -> BTreeMap<String, BTreeSet<String>> {
    let class = regex::Regex::new(r"^---@class ([\w.]+)").unwrap();
    let local = regex::Regex::new(r"^local (\w+) = \{\}").unwrap();
    let method = regex::Regex::new(r"^function (\w+):(\w+)\(").unwrap();
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (_, text) in read_all(dir) {
        let (mut pending, mut locals) = (None::<String>, BTreeMap::<String, String>::new());
        for line in text.lines() {
            if let Some(c) = class.captures(line) {
                pending = Some(c[1].to_owned());
            } else if let Some(c) = local.captures(line) {
                if let Some(class) = pending.take() {
                    locals.insert(c[1].to_owned(), class);
                }
            } else if !line.starts_with("---") {
                pending = None;
            }
            if let Some(c) = method.captures(line)
                && let Some(class) = locals.get(&c[1])
            {
                out.entry(class.clone()).or_default().insert(c[2].to_owned());
            }
        }
    }
    out
}

/// Per service, never pooled: every non-remote method in a service contract must be declared on
/// every LuaLS class that types that service, and the class declares nothing outside the contract.
fn luals_problems(
    contracts: &BTreeMap<String, mare_kernel::ServiceContract>,
    classes: &BTreeMap<String, BTreeSet<String>>,
    public: &Methods,
) -> Vec<String> {
    let mut problems = vec![];
    for (service, contract) in contracts {
        if contract.owner.is_some() {
            continue;
        }
        let Some(class_set) = classes.get(service) else {
            problems.push(format!("ctx.lua types no class for service `{service}`"));
            continue;
        };
        // a service typed by several classes (a ctx field, an inject overload): each class is a
        // way in, so each must declare the whole contract on its own (never their union)
        for class in class_set {
            let declared = public.get(class).cloned().unwrap_or_default();
            class_problems(&mut problems, service, contract, class, &declared);
        }
    }
    problems
}

fn class_problems(
    problems: &mut Vec<String>,
    service: &str,
    contract: &mare_kernel::ServiceContract,
    class: &str,
    declared: &BTreeSet<String>,
) {
    for method in contract.methods.keys() {
        if !declared.contains(method) {
            problems.push(format!("{service}.{method} is not declared on {class}"));
        }
    }
    for method in declared {
        if !contract.methods.contains_key(method) {
            problems.push(format!("{class}:{method} is declared but not in the `{service}` contract"));
        }
    }
}

type Methods = BTreeMap<String, BTreeSet<String>>;

/// Service -> the classes ctx.lua types it with: `---@field <service> <Class>` and
/// `---@overload fun(self: mare.Ctx, name: "<service>"): <Class>|nil` (ctx:inject).
fn luals_inputs() -> (BTreeMap<String, BTreeSet<String>>, Methods) {
    let ctx = std::fs::read_to_string(preset().join("types/mare/services/ctx.lua")).unwrap();
    let field = regex::Regex::new(r"(?m)^---@field ([\w-]+) ([\w.]+)").unwrap();
    let inject =
        regex::Regex::new(r#"(?m)^---@overload fun\(self: mare\.Ctx, name: "([\w-]+)"\): ([\w.]+)\|nil"#).unwrap();
    let mut classes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for c in field.captures_iter(&ctx).chain(inject.captures_iter(&ctx)) {
        classes.entry(c[1].to_owned()).or_default().insert(c[2].to_owned());
    }
    (classes, luals_methods("types"))
}

#[test]
fn contract_methods_and_luals_service_types_agree() {
    let (classes, public) = luals_inputs();
    let problems = luals_problems(&contracts(), &classes, &public);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Counter-test: name-only contracts (`required`/`optional`) are rejected by the parser, and
/// dropping a typed contract method (`permissions.set`) from its LuaLS class fails parity.
#[test]
fn name_only_contracts_are_rejected_and_permissions_is_checked_against_luals() {
    let lua = mlua::Lua::new();
    let legacy: mlua::Value = lua.load("return { m = { required = { 'status' } } }").eval().unwrap();
    assert!(mare_kernel::contracts::parse_contracts(&lua, legacy).is_err(), "name-only contract must be rejected");

    let contracts = contracts();
    let permissions = &contracts["permissions"];
    assert!(permissions.methods.contains_key("set"));
    let (classes, mut public) = luals_inputs();
    let class = classes["permissions"].iter().next().unwrap().clone();
    assert_eq!(classes["permissions"].len(), 1);
    assert!(public.get_mut(&class).unwrap().remove("set"), "{class} declares set");
    let problems = luals_problems(&contracts, &classes, &public);
    assert_eq!(problems, [format!("permissions.set is not declared on {class}")]);
}

/// Counter-test: a service typed by two classes (a ctx field and an inject overload) is checked
/// class by class; a method missing from one is not hidden by the other.
#[test]
fn every_class_of_a_service_declares_its_contract() {
    let lua = mlua::Lua::new();
    let value: mlua::Value = lua
        .load("return { m = { methods = { status = { doc = 's', result = { type = 'string' } }, list = { doc = 'l', result = { type = 'array' } } } } }")
        .eval()
        .unwrap();
    let contracts = mare_kernel::contracts::parse_contracts(&lua, value).unwrap();
    let classes: BTreeMap<String, BTreeSet<String>> =
        [("m".into(), ["A".into(), "B".into()].into())].into_iter().collect();
    let both: Methods =
        [("A".into(), ["status".into(), "list".into()].into()), ("B".into(), ["status".into(), "list".into()].into())]
            .into_iter()
            .collect();
    assert!(luals_problems(&contracts, &classes, &both).is_empty());
    let split: Methods =
        [("A".into(), ["status".into()].into()), ("B".into(), ["status".into(), "list".into()].into())]
            .into_iter()
            .collect();
    let problems = luals_problems(&contracts, &classes, &split);
    assert_eq!(problems, ["m.list is not declared on A"]);
}

/// A contract's `errors` define the service's own codes (`name = { doc, data? }`, data a JSON
/// Schema), raised as `<service>.<name>`; a method's `errors` lists the ones it raises.
#[test]
fn contract_errors_are_declared_names() {
    let parse = |errors: &str, method: &str| {
        let lua = mlua::Lua::new();
        let value: mlua::Value = lua
            .load(format!(
                "return {{ s = {{ errors = {errors}, methods = {{ f = {{ doc = 'f', result = {{ type = 'object' }}{method} }} }} }} }}"
            ))
            .eval()
            .unwrap();
        mare_kernel::contracts::parse_contracts(&lua, value)
    };
    let good = "{ not_found = { doc = 'no such thing', data = { type = 'object' } }, busy = { doc = 'later' } }";
    let contracts = parse(good, ", errors = { 'busy' }").unwrap();
    let declared = mare_kernel::contracts::declared_errors("s", &contracts["s"]);
    assert_eq!(declared["s.not_found"], json!({ "doc": "no such thing", "data": { "type": "object" } }));
    assert_eq!(declared["s.busy"], json!({ "doc": "later" }));
    for bad in [
        "{ NotFound = { doc = 'x' } }",
        "{ ['not-found'] = { doc = 'x' } }",
        "{ not_found = 'x' }",
        "{ not_found = { doc = 'x', data = { type = 'nope' } } }",
        "{ not_found = { doc = 'x', extra = 1 } }",
        "'x'",
    ] {
        assert!(parse(bad, "").is_err(), "{bad} was accepted");
    }
    assert!(parse(good, ", errors = { 'missing' }").is_err(), "a method listed an undeclared error");
}

/// Counter-test: two services with a same-named Lua-only method; dropping one service's declaration
/// must fail even though the other service still declares the name.
#[test]
fn a_same_named_lua_method_of_another_service_does_not_hide_a_missing_declaration() {
    let lua = mlua::Lua::new();
    let value: mlua::Value = lua
        .load("return { a = { methods = { snap = { doc = 's', expose = 'lua', result = { type = 'object' } } } }, b = { methods = { snap = { doc = 's', expose = 'lua', result = { type = 'object' } } } } }")
        .eval()
        .unwrap();
    let contracts = mare_kernel::contracts::parse_contracts(&lua, value).unwrap();
    let classes: BTreeMap<String, BTreeSet<String>> =
        [("a".into(), ["A".into()].into()), ("b".into(), ["B".into()].into())].into_iter().collect();
    let both: BTreeMap<String, BTreeSet<String>> =
        [("A".into(), ["snap".into()].into()), ("B".into(), ["snap".into()].into())].into_iter().collect();
    assert!(luals_problems(&contracts, &classes, &both).is_empty());
    let only_b: BTreeMap<String, BTreeSet<String>> = [("B".into(), ["snap".into()].into())].into_iter().collect();
    let problems = luals_problems(&contracts, &classes, &only_b);
    assert_eq!(problems, ["a.snap is not declared on A"]);
}

/// `ui` is `mare.ui`: one name for both spellings.
fn normalize(name: &str) -> String {
    name.strip_prefix("mare.ui.").map(|rest| format!("ui.{rest}")).unwrap_or_else(|| name.to_owned())
}

/// Functions the hand-written primitive types declare: `function mare.x.y(`, `function ui.Y:z(`.
fn declared_primitives() -> BTreeSet<String> {
    let re = regex::Regex::new(r"(?m)^function ((?:mare|ui)(?:[.:][A-Za-z_][A-Za-z0-9_]*)+)\s*\(").unwrap();
    read_all("types")
        .iter()
        .flat_map(|(_, text)| re.captures_iter(text).map(|c| normalize(&c[1].replace(':', "."))).collect::<Vec<_>>())
        .collect()
}

/// Declared primitives that do not exist in a plugin's runtime, and why.
const NOT_IN_PLUGIN_RUNTIME: &[(&str, &str)] = &[
    // installed by the host's approval path when an agent runs tools
    ("mare.subagent.permission_commit", "installed by the host approval path"),
    ("mare.subagent.permission_lift", "installed by the host approval path"),
    ("mare.subagent.rule_lift", "installed by the host approval path"),
    // installed when a frontend is attached
    ("ui.approve", "installed with a frontend"),
    ("ui.confirm", "installed with a frontend"),
    ("ui.editor", "installed with a frontend"),
    ("ui.input", "installed with a frontend"),
    ("ui.questions", "installed with a frontend"),
    ("ui.select", "installed with a frontend"),
];

/// Declared config primitives checked in init.lua. `caller_site` is also used to track runtime writes.
const CONFIG_ONLY: &[&str] = &[
    "mare.caller_site",
    "mare.command.alias",
    "mare.model",
    "mare.params",
    "mare.prompt.remove",
    "mare.prompt.section",
    "mare.provider",
    "mare.status.hook",
    "mare.tool.command",
    "mare.tool.describe",
    "mare.tools.disable",
];

/// The last line of the init.lua checks: reaching it proves every check before it ran and held.
const INIT_RAN: &str = "api parity: every init.lua check ran";

/// Run `assert(type(name) == 'function')` for each name in a real init.lua; the config error
/// the host reports (the file always ends by raising INIT_RAN).
async fn init_lua_checks(names: &[&str]) -> String {
    let env = TestEnv::new().await;
    let mut checks: String = names
        .iter()
        .map(|name| format!("assert(type({name}) == 'function', '{name} is not a function in init.lua')\n"))
        .collect();
    let _ = writeln!(checks, "error('{INIT_RAN}')");
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(env.config_dir().join("init.lua"), checks).unwrap();
    // a config error is the expected outcome here, so not the builder's no-problems host
    let host = mare_host::Host::new(env.host().user_config(false).faux(json!([])).opts).await.unwrap();
    let error = host.config_error().expect("init.lua ran and raised its last line");
    host.shutdown().await;
    error
}

/// Counter-test: a config-time function that does not exist fails the init.lua checks, which
/// therefore really run.
#[tokio::test]
async fn a_missing_config_time_function_fails_the_init_lua_checks() {
    local(async {
        let error = init_lua_checks(&["mare.model", "mare.no_such_config_function"]).await;
        assert!(error.contains("mare.no_such_config_function is not a function in init.lua"), "{error}");
        assert!(!error.contains(INIT_RAN), "{error}");
    })
    .await
}

#[tokio::test]
async fn exec_path_helpers_are_not_public_runtime_functions() {
    local(async {
        let env = TestEnv::new().await;
        let host = env.host().faux(json!([])).no_session().build().await;
        let (process_private, nul_private): (bool, bool) =
            eval(&host, "return mare.exec.check_process_path == nil, mare.exec.reject_nul_path == nil").await;
        assert!(process_private, "the stdlib process-path checker must not be on mare.exec");
        assert!(nul_private, "the stdlib NUL-path checker must not be on mare.exec");
        host.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn caller_site_replaces_the_legacy_underscore_helper() {
    local(async {
        let error = init_lua_checks(&["mare.caller_site"]).await;
        assert!(error.contains(INIT_RAN) && !error.contains("is not a function"), "{error}");
        let env = TestEnv::new().await;
        let host = env.host().faux(json!([])).no_session().build().await;
        let (new_exists, old_absent): (bool, bool) =
            eval(&host, "return type(mare.caller_site) == 'function', mare._where == nil").await;
        assert!(new_exists, "runtime option-source tracking still needs caller_site");
        assert!(old_absent, "the old underscore alias must be removed");
        host.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn primitive_types_match_the_runtime_both_ways() {
    local(async {
        // the init.lua-only functions exist where they are documented to
        let error = init_lua_checks(CONFIG_ONLY).await;
        assert!(error.contains(INIT_RAN) && !error.contains("is not a function"), "{error}");
        let env = TestEnv::new().await;
        let host = env.host().faux(json!([])).build().await;
        let runtime: String = eval(
            &host,
            r#"
            local out, seen = {}, {}
            local function walk(prefix, t, depth)
                if seen[t] or depth > 4 then return end
                seen[t] = true
                for k, v in pairs(t) do
                    -- `super` is a class's inheritance link (mare.Service -> cordis.Service): its
                    -- methods are the parent's, typed under types/cordis
                    if type(k) == "string" and not k:match("^_") and k ~= "super" then
                        local name = prefix .. "." .. k
                        if type(v) == "function" then out[#out + 1] = name
                        elseif type(v) == "table" then walk(name, v, depth + 1) end
                    end
                end
            end
            walk("mare", mare, 1)
            walk("ui", ui, 1)
            table.sort(out)
            return table.concat(out, "\n")
        "#,
        )
        .await;
        let runtime: BTreeSet<String> = runtime.lines().map(normalize).collect();
        let declared = declared_primitives();
        let absent: BTreeSet<&str> =
            NOT_IN_PLUGIN_RUNTIME.iter().map(|(n, _)| *n).chain(CONFIG_ONLY.iter().copied()).collect();
        let lies: Vec<_> = declared.iter().filter(|n| !runtime.contains(*n) && !absent.contains(n.as_str())).collect();
        let hidden: Vec<_> = runtime.iter().filter(|n| !declared.contains(*n)).collect();
        assert!(lies.is_empty(), "typed but not in the runtime: {lies:?}");
        assert!(hidden.is_empty(), "public at runtime but untyped: {hidden:?}");
        // the exemptions stay true: they shrink, never rot
        for name in &absent {
            assert!(declared.contains(*name), "exemption `{name}` is no longer declared: drop it");
        }
        host.shutdown().await;
    })
    .await
}

fn service_classes() -> BTreeMap<String, String> {
    let ctx = std::fs::read_to_string(preset().join("types/mare/services/ctx.lua")).unwrap();
    let field = regex::Regex::new(r"(?m)^---@field ([\w-]+) ([\w.]+)").unwrap();
    let inject =
        regex::Regex::new(r#"(?m)^---@overload fun\(self: mare\.Ctx, name: "([\w-]+)"\): ([\w.]+)\|nil"#).unwrap();
    let mut classes = BTreeMap::new();
    for c in field.captures_iter(&ctx).chain(inject.captures_iter(&ctx)) {
        classes.entry(c[1].to_owned()).or_insert_with(|| c[2].to_owned());
    }
    classes
}

/// The LuaLS service types generated from the contracts are current:
/// `types/mare/services/<service>.gen.lua` equals what the contract generates now, and no
/// generated file outlives its contract. `UPDATE_GENERATED=1` rewrites them.
#[test]
fn generated_luals_are_current() {
    let dir = preset().join("types/mare/services");
    let classes = service_classes();
    let mut expected = BTreeSet::new();
    for (service, contract) in contracts().iter().filter(|(_, c)| c.owner.is_none()) {
        let class = classes.get(service).unwrap_or_else(|| panic!("ctx.lua has no field for `{service}`"));
        let text = mare_kernel::contracts::luals_types(service, class, contract);
        let path = dir.join(format!("{service}.gen.lua"));
        expected.insert(path.clone());
        if std::env::var_os("UPDATE_GENERATED").is_some() {
            std::fs::write(&path, &text).unwrap();
        }
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            current == text,
            "{} is stale: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current",
            path.display()
        );
    }
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.to_string_lossy().ends_with(".gen.lua") {
            assert!(expected.contains(&path), "{} has no contract behind it", path.display());
        }
    }
    // generated event declarations (`types/mare/events.gen.lua`) stay byte-for-byte current.
    let events_path = preset().join("types/mare/events.gen.lua");
    let events_text = mare_kernel::contracts::luals_events(&contracts());
    if std::env::var_os("UPDATE_GENERATED").is_some() {
        std::fs::write(&events_path, &events_text).unwrap();
    }
    let events_current = std::fs::read_to_string(&events_path).unwrap_or_default();
    assert!(
        events_current == events_text,
        "{} is stale: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current",
        events_path.display()
    );
}

/// Counter-test: changing only a parameter's or the result's type (not a method name) changes
/// the generated declaration, so the freshness check fails on a stale signature.
#[test]
fn a_type_only_change_changes_the_generated_declaration() {
    let render = |param: &str, result: &str| {
        let lua = mlua::Lua::new();
        let value: mlua::Value = lua
            .load(format!(
                "return {{ s = {{ methods = {{ f = {{ doc = 'f', expose = 'both', order = {{ 'x' }}, \
                 params = {{ type = 'object', properties = {{ x = {{ type = '{param}' }} }}, additionalProperties = false }}, \
                 result = {{ type = '{result}' }} }} }} }} }}"
            ))
            .eval()
            .unwrap();
        let contracts = mare_kernel::contracts::parse_contracts(&lua, value).unwrap();
        mare_kernel::contracts::luals_types("s", "S", &contracts["s"])
    };
    let base = render("string", "string");
    assert!(
        base.contains("---@param x? string")
            && base.contains("---@return string")
            && base.contains("function M:f(x) end")
    );
    assert_ne!(base, render("integer", "string"), "a parameter type change must show");
    assert_ne!(base, render("string", "boolean"), "a result type change must show");
}

/// One-method contract `s.f` from a Lua literal of the method's fields.
fn render_method(fields: &str) -> String {
    let lua = mlua::Lua::new();
    let value: mlua::Value = lua
        .load(format!("return {{ s = {{ methods = {{ f = {{ doc = 'f', expose = 'both', {fields} }} }} }} }}"))
        .eval()
        .unwrap();
    let contracts = mare_kernel::contracts::parse_contracts(&lua, value).unwrap();
    mare_kernel::contracts::luals_types("s", "S", &contracts["s"])
}

/// Counter-test: a field of the trailing opts becoming required changes the declaration (opts
/// is then required too), so a stale optional signature fails the freshness check.
#[test]
fn a_required_trailing_option_changes_the_generated_declaration() {
    let render = |required: &str| {
        render_method(&format!(
            "result = {{ type = 'string' }}, order = {{ 'x', '...' }}, params = {{ type = 'object', properties = {{ x = {{ type = 'string' }}, \
             save = {{ type = 'boolean' }} }}, {required}additionalProperties = false }}"
        ))
    };
    // without a `required` list, and with save required
    let optional = render("");
    assert!(optional.contains("---@param opts? {save?: boolean}"), "{optional}");
    let required = render("required = { 'save' }, ");
    assert!(required.contains("---@param opts {save: boolean}"), "{required}");
}

/// Counter-test: an object with named fields also types its other fields
/// (`additionalProperties`); changing only that type changes the declaration.
#[test]
fn an_additional_properties_type_change_changes_the_generated_declaration() {
    let render = |extra: &str| {
        render_method(&format!(
            "result = {{ type = 'object', properties = {{ a = {{ type = 'string' }} }}, \
             additionalProperties = {{ type = '{extra}' }} }}"
        ))
    };
    let base = render("string");
    assert!(base.contains("---@return {a?: string, [string]: string}"), "{base}");
    assert_ne!(base, render("integer"), "an additionalProperties type change must show");
}

/// Counter-test: a schema titled with a LuaLS class (`title = "mare.X"`, a JSON Schema
/// annotation with no RPC meaning) is declared as that class.
#[test]
fn a_titled_schema_is_declared_as_its_class() {
    let render = |title: &str| {
        render_method(&format!(
            "result = {{ type = 'object', {title}properties = {{ a = {{ type = 'string' }} }}, additionalProperties = false }}"
        ))
    };
    assert!(render("").contains("---@return {a?: string}"));
    assert!(render("title = 'mare.Thing', ").contains("---@return mare.Thing"));
}

/// A class's fields: name -> (optional `?`, LuaLS type).
type ClassFields = BTreeMap<String, BTreeMap<String, (bool, String)>>;

/// The `---@field` lines of every class in `types/`.
fn class_fields() -> ClassFields {
    let field = regex::Regex::new(r"^---@field (\w+)(\??) (\S+)").unwrap();
    let class = regex::Regex::new(r"^---@class ([\w.]+)").unwrap();
    let mut fields = ClassFields::new();
    for (_, text) in read_all("types") {
        let mut current = None;
        for line in text.lines() {
            if let Some(c) = class.captures(line) {
                current = Some(c[1].to_owned());
            } else if let (Some(c), Some(name)) = (field.captures(line), &current) {
                fields.entry(name.clone()).or_default().insert(c[1].to_owned(), (&c[2] == "?", c[3].to_owned()));
            } else if !line.starts_with("---") {
                current = None;
            }
        }
    }
    fields
}

/// Where a titled schema and its class must differ: a class type richer than JSON Schema can say.
/// The class must declare exactly this type, and every entry must still be in use.
const RICHER_THAN_SCHEMA: &[(&str, &str, &str)] =
    &[("mare.ThemeSnapshot", "styles", "table<string,mare.Style>"), ("mare.ThemeSnapshot", "component", "mare.Node")];

/// A schema titled with a LuaLS class and that class describe the same fields (names, which are
/// optional, and types), so returning the class name loses nothing.
fn titled_class_problems(
    contracts: &BTreeMap<String, mare_kernel::ServiceContract>,
    classes: &ClassFields,
) -> (usize, Vec<String>) {
    let mut problems = vec![];
    let mut titled = 0;
    for (service, contract) in contracts {
        for (method, spec) in &contract.methods {
            let Some(title) = spec.result.get("title").and_then(Value::as_str) else { continue };
            titled += 1;
            let properties = spec.result["properties"].as_object().unwrap();
            let required: BTreeSet<&str> =
                spec.result["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let fields = classes.get(title).cloned().unwrap_or_default();
            let declared: BTreeSet<&String> = fields.keys().collect();
            let expected: BTreeSet<&String> = properties.keys().collect();
            if declared != expected {
                problems.push(format!("{service}.{method}: class {title} fields {declared:?} vs schema {expected:?}"));
            }
            let compact = |t: &str| t.chars().filter(|c| !c.is_whitespace()).collect::<String>();
            for (name, schema) in properties {
                let Some((optional, kind)) = fields.get(name) else { continue };
                if *optional == required.contains(name.as_str()) {
                    problems.push(format!(
                        "{title}.{name} is {} in the class, {} in the {service}.{method} schema",
                        if *optional { "optional" } else { "required" },
                        if required.contains(name.as_str()) { "required" } else { "optional" },
                    ));
                }
                let richer = RICHER_THAN_SCHEMA.iter().find(|(c, f, _)| *c == title && f == name).map(|(_, _, t)| *t);
                let want =
                    richer.map(str::to_owned).unwrap_or_else(|| mare_kernel::contracts::luals_schema_type(schema));
                if compact(kind) != compact(&want) {
                    problems.push(format!(
                        "{title}.{name} is `{kind}` in the class, `{want}` from the {service}.{method} schema"
                    ));
                }
            }
        }
    }
    for (class, name, _) in RICHER_THAN_SCHEMA {
        if !classes.get(*class).is_some_and(|f| f.contains_key(*name)) {
            problems.push(format!("exception {class}.{name} is no longer a field: drop it"));
        }
    }
    (titled, problems)
}

#[test]
fn titled_schemas_and_their_classes_have_the_same_fields() {
    let (titled, problems) = titled_class_problems(&contracts(), &class_fields());
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    assert!(titled >= 5, "theme (4) + status.get (1) return titled snapshots");
}

/// Counter-test: a class field that becomes optional, or changes type, while the schema keeps it
/// required and typed, fails the check (the reviewer's revision integer -> revision? string).
#[test]
fn a_class_field_that_drifts_from_its_schema_fails_the_check() {
    let contracts = contracts();
    let pristine = class_fields();
    assert!(titled_class_problems(&contracts, &pristine).1.is_empty());
    for (optional, kind) in [(true, "integer"), (false, "string"), (true, "string")] {
        let mut drifted = pristine.clone();
        drifted.get_mut("mare.ThemeSnapshot").unwrap().insert("revision".into(), (optional, kind.into()));
        let problems = titled_class_problems(&contracts, &drifted).1;
        assert!(
            problems.iter().any(|p| p.contains("revision")),
            "revision{} {kind} passed",
            if optional { "?" } else { "" }
        );
    }
    // a richer type outside the registered exceptions is drift too
    let mut drifted = pristine.clone();
    drifted.get_mut("mare.ThemeSnapshot").unwrap().insert("icons".into(), (false, "table<string,mare.Style>".into()));
    assert!(titled_class_problems(&contracts, &drifted).1.iter().any(|p| p.contains("icons")));
}

/// The Lua-only types the hand-written declarations had stay in the generated ones: login's
/// callback signatures, the quota fields of rate limits.
#[test]
fn generated_declarations_keep_lua_only_types() {
    let dir = preset().join("types/mare/services");
    let llm = std::fs::read_to_string(dir.join("llm.gen.lua")).unwrap();
    for callback in [
        "say?: fun(text: string)",
        "ask?: (fun(title: string, opts?: table): string|nil)",
        "choose?: (fun(title: string, items: table[]): integer|nil)",
        "tool_update?: \"addition_only\"|false",
    ] {
        assert!(llm.contains(callback), "llm.gen.lua lost `{callback}`");
    }
    let theme = std::fs::read_to_string(dir.join("theme.gen.lua")).unwrap();
    assert_eq!(
        theme.matches("---@return mare.ThemeSnapshot\n").count(),
        4,
        "theme snapshots keep their class: {theme}"
    );
    let status = std::fs::read_to_string(dir.join("status.gen.lua")).unwrap();
    assert!(status.contains("---@return mare.Status\nfunction M:get() end"), "status.gen.lua: {status}");
    let usage = std::fs::read_to_string(dir.join("usage.gen.lua")).unwrap();
    let window = "{reset_at_ms?: integer, used_percent: number, window_minutes?: number, [string]: any}";
    assert!(usage.contains("reached?: string"), "usage.gen.lua lost `reached?: string`");
    for field in [
        format!("primary?: {window}"),
        format!("secondary?: {window}"),
        "credits?: string".to_owned(),
        "reached?: string".to_owned(),
    ] {
        assert!(usage.contains(&field), "usage.gen.lua lost `{field}`");
    }
    let mcp = std::fs::read_to_string(dir.join("mcp.gen.lua")).unwrap();
    assert!(mcp.contains("mare.McpServerStatus"), "mcp.gen.lua lost mare.McpServerStatus: {mcp}");
    let terminal = std::fs::read_to_string(dir.join("terminal.gen.lua")).unwrap();
    assert!(terminal.contains("terminal_id"), "terminal.gen.lua must use terminal_id");
    assert!(!terminal.contains("session_id"), "terminal.gen.lua must not keep session_id");
}

#[test]
fn jobs_contract_declares_derived_rpc_and_access_levels() {
    use mare_kernel::contracts::{Access, Expose};
    let all = contracts();
    let jobs = all.get("jobs").expect("jobs contract must exist");
    assert!(jobs.owner.is_none(), "jobs must have a full contract, not name-only");
    for (method, expected_access) in
        [("list", Access::Peer), ("read", Access::Peer), ("background", Access::Peer), ("cancel", Access::Peer)]
    {
        let spec = &jobs.methods[method];
        assert_eq!(spec.expose, Expose::Both, "jobs.{method} must be expose = both");
        assert_eq!(spec.access, expected_access, "jobs.{method} access mismatch");
    }
}

#[test]
fn status_contract_is_full_and_derived() {
    let all = contracts();
    let status = all.get("status").expect("status contract missing");
    assert!(!status.methods.is_empty(), "status must have a full contract, not name-only");
    let get = status.methods.get("get").expect("status.get missing from contract");
    assert_eq!(get.expose, mare_kernel::contracts::Expose::Both);
    assert_eq!(get.access, mare_kernel::contracts::Access::PeerDefault);
    assert_eq!(get.scope, mare_kernel::contracts::Scope::Host);
    assert_eq!(get.result.get("title").and_then(Value::as_str), Some("mare.Status"));

    let derived_methods = derived();
    assert!(derived_methods.contains_key("status.get"), "status.get must be derived from contracts");

    let proto_schema = mare_protocol::schema();
    let proto_methods = proto_schema["methods"].as_object().unwrap();
    assert!(!proto_methods.contains_key("status.get"), "status.get must leave hand-written mare-protocol schema");
}

#[test]
fn query_contract_is_full_and_derived() {
    let all = contracts();
    let query = all.get("query").expect("query contract missing");
    assert!(!query.methods.is_empty(), "query must have a full contract, not name-only");

    let call = query.methods.get("call").expect("query.call missing from contract");
    assert_eq!(call.expose, mare_kernel::contracts::Expose::Both);
    assert_eq!(call.access, mare_kernel::contracts::Access::Peer);

    let providers = query.methods.get("list_providers").expect("query.list_providers missing from contract");
    assert_eq!(providers.expose, mare_kernel::contracts::Expose::Both);
    assert_eq!(providers.access, mare_kernel::contracts::Access::Peer);

    let supports = query.methods.get("supports").expect("query.supports missing from contract");
    assert_eq!(supports.expose, mare_kernel::contracts::Expose::Lua);

    let register = query.methods.get("register").expect("query.register missing from contract");
    assert_eq!(register.expose, mare_kernel::contracts::Expose::Lua);
    assert_eq!(register.access, mare_kernel::contracts::Access::Primary);
    assert!(register.registration);

    let derived_methods = derived();
    assert!(derived_methods.contains_key("query.call"), "query.call must be derived from contracts");
    assert!(
        derived_methods.contains_key("query.list_providers"),
        "query.list_providers must be derived from contracts"
    );

    let proto_schema = mare_protocol::schema();
    let proto_methods = proto_schema["methods"].as_object().unwrap();
    assert!(!proto_methods.contains_key("query.call"), "query.call must leave mare-protocol schema");
    assert!(
        !proto_methods.contains_key("query.list_providers"),
        "query.list_providers must leave mare-protocol schema"
    );
}

#[test]
fn mcp_contract_declares_renamed_rpc_and_access_levels() {
    use mare_kernel::contracts::{Access, Expose};
    let all = contracts();
    let mcp = all.get("mcp").expect("mcp contract must exist");
    assert!(mcp.owner.is_none(), "mcp must have a full contract, not name-only");
    assert_eq!(mcp.methods["catalog"].expose, Expose::Lua);
    for (method, expected_access) in [
        ("list_servers", Access::Peer),
        ("list_tools", Access::Peer),
        ("list_resources", Access::Peer),
        ("read_resource", Access::Peer),
        ("list_prompts", Access::Peer),
        ("get_prompt", Access::Peer),
        ("connect", Access::Primary),
        ("reconnect", Access::Primary),
    ] {
        let spec = &mcp.methods[method];
        assert_eq!(spec.expose, Expose::Both, "mcp.{method} must be expose = both");
        assert_eq!(spec.access, expected_access, "mcp.{method} access mismatch");
    }
}

#[test]
fn terminal_contract_declares_terminal_id_access_and_max_bytes() {
    let all = contracts();
    let term = all.get("terminal").expect("terminal contract must exist");
    assert!(term.owner.is_none(), "terminal must have a full contract, not name-only");

    for (method, expected_access) in [
        ("create", "primary"),
        ("send", "primary"),
        ("read", "peer"),
        ("signal", "primary"),
        ("resize", "primary"),
        ("close", "primary"),
        ("list", "peer"),
    ] {
        let spec = term.methods.get(method).unwrap_or_else(|| panic!("missing terminal.{method}"));
        assert!(spec.remote(), "terminal.{method} must be remote-exposed");
        assert_eq!(spec.access_name(), expected_access, "terminal.{method} access level");
    }

    for method in ["send", "read", "signal", "resize", "close"] {
        let params = &term.methods[method].params;
        assert!(
            params.pointer("/properties/terminal_id").is_some(),
            "terminal.{method} params must declare terminal_id"
        );
        assert!(
            params.pointer("/properties/session_id").is_none(),
            "terminal.{method} params must not keep session_id"
        );
    }

    for method in ["send", "read"] {
        let desc = term.methods[method]
            .params
            .pointer("/properties/max_bytes/description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(!desc.is_empty(), "terminal.{method} max_bytes must have contract description");
    }
}

#[test]
fn fs_contract_is_full_and_derived() {
    use mare_kernel::contracts::{Access, Expose};
    let contracts = contracts();
    let fs = contracts.get("fs").expect("fs contract missing");
    assert!(fs.owner.is_none(), "fs must have a full contract, not name-only");
    for (method, expected_access) in
        [("read", Access::Peer), ("write", Access::Primary), ("list", Access::Peer), ("stat", Access::Peer)]
    {
        let spec = fs.methods.get(method).unwrap_or_else(|| panic!("fs.{method} missing from contracts/fs.lua"));
        assert_eq!(spec.expose, Expose::Both, "fs.{method} must be remotely exposed");
        assert_eq!(spec.access, expected_access, "fs.{method} access mismatch");
    }
    assert!(!mare_kernel::contracts::PROTOCOL_ROOTS.contains(&"fs"), "`fs` must be removed from PROTOCOL_ROOTS");
    let derived = derived();
    for (method, expected_schema_key) in
        [("fs.read", "data_base64"), ("fs.write", "data_base64"), ("fs.list", "data"), ("fs.stat", "is_file")]
    {
        let entry = derived.get(method).unwrap_or_else(|| panic!("{method} missing from derived contract schema"));
        let serialized = entry.to_string();
        assert!(
            serialized.contains(expected_schema_key),
            "{method} schema missing `{expected_schema_key}`: {serialized}"
        );
    }
    let generated =
        std::fs::read_to_string(preset().join("types/mare/services/fs.gen.lua")).expect("fs.gen.lua missing");
    assert!(generated.contains("---@class mare.FsService"), "{generated}");
}

#[test]
fn pkg_contract_is_full_and_derived() {
    use mare_kernel::contracts::Access;
    let contracts = contracts();
    let pkg = contracts.get("plugins").expect("plugins contract missing");
    assert!(pkg.owner.is_none(), "plugins must be a full contract");
    for (method, expected_access) in [
        ("list", Access::Peer),
        ("install", Access::Primary),
        ("update", Access::Primary),
        ("uninstall", Access::Primary),
    ] {
        let spec = pkg.methods.get(method).unwrap_or_else(|| panic!("plugins.{method} missing"));
        assert!(spec.remote(), "plugins.{method} must be remote");
        assert!(!spec.doc.is_empty(), "plugins.{method} must be documented");
        assert_eq!(spec.access, expected_access, "plugins.{method} access mismatch");
    }
    assert!(pkg.methods.contains_key("news"), "plugins.news must be declared");
    assert!(pkg.methods.contains_key("choose_plugins"), "plugins.choose_plugins must be declared");
    assert!(pkg.errors.contains_key("failed"), "plugins contract must declare `failed` error");
    assert!(!mare_kernel::contracts::PROTOCOL_ROOTS.contains(&"packages"), "`packages` must leave PROTOCOL_ROOTS");

    let schema = rpc_schema();
    let methods = schema["methods"].as_object().unwrap();
    for added in ["plugins.list", "plugins.install", "plugins.update", "plugins.uninstall"] {
        assert!(methods.contains_key(added), "{added} missing from mare rpc schema");
    }
}

#[test]
fn spill_contract_is_full_and_derived() {
    let all = contracts();
    assert!(!all.contains_key("output"), "output contract must be removed when renamed to spill");
    let spill = all.get("spill").expect("spill contract missing");
    assert!(!spill.methods.is_empty(), "spill must have a full contract, not name-only");

    let list_backends = spill.methods.get("list_backends").expect("spill.list_backends missing");
    assert_eq!(list_backends.expose, mare_kernel::contracts::Expose::Both);
    assert_eq!(list_backends.access, mare_kernel::contracts::Access::Peer);
    assert_eq!(list_backends.scope, mare_kernel::contracts::Scope::Host);

    for (name, expected_access) in [
        ("save", mare_kernel::contracts::Access::Primary),
        ("read", mare_kernel::contracts::Access::Peer),
        ("delete", mare_kernel::contracts::Access::Primary),
    ] {
        let method = spill.methods.get(name).unwrap_or_else(|| panic!("spill.{name} missing"));
        assert_eq!(method.expose, mare_kernel::contracts::Expose::Both, "{name}");
        assert_eq!(method.access, expected_access, "{name}");
        assert_eq!(method.scope, mare_kernel::contracts::Scope::Session, "{name}");
    }

    for name in ["register", "save_owned", "save_bytes", "read_owned", "read_bytes", "delete_owned"] {
        let method = spill.methods.get(name).unwrap_or_else(|| panic!("spill.{name} missing"));
        assert_eq!(method.expose, mare_kernel::contracts::Expose::Lua, "{name}");
        assert!(!method.access_declared, "local-only {name} must not declare access");
        if name == "register" {
            assert!(method.registration);
        }
    }

    let derived_methods = derived();
    for name in ["spill.list_backends", "spill.save", "spill.read", "spill.delete"] {
        assert!(derived_methods.contains_key(name), "{name} must be derived from contracts");
    }
    assert!(!derived_methods.keys().any(|k| k.starts_with("output.")), "derived methods must not contain output.*");

    let proto_schema = mare_protocol::schema();
    let proto_methods = proto_schema["methods"].as_object().unwrap();
    assert!(!proto_methods.keys().any(|k| k.starts_with("output.")), "output.* must leave mare-protocol schema");
}

/// `web` has a full contract in `contracts/web.lua` with
/// `web.{list_providers,search,fetch}` derived from it, `web.{providers,selection}` removed from
/// both contracts and `mare-protocol`, and `register_{search,fetch}` kept Lua-only.
#[test]
fn web_contract_is_full_and_derived() {
    let all = contracts();
    let web = all.get("web").expect("web missing from service contracts");
    assert!(!web.methods.is_empty(), "web must have a full contract, not name-only");

    let list_providers = web.methods.get("list_providers").expect("web.list_providers missing from contract");
    assert_eq!(list_providers.expose, mare_kernel::contracts::Expose::Both);
    assert_eq!(list_providers.access, mare_kernel::contracts::Access::Peer);

    for call_method in ["search", "fetch"] {
        let spec = web.methods.get(call_method).unwrap_or_else(|| panic!("web.{call_method} missing from contract"));
        assert_eq!(spec.expose, mare_kernel::contracts::Expose::Both);
        assert_eq!(spec.access, mare_kernel::contracts::Access::Peer);
        assert!(spec.confined, "web.{call_method} must run confined");
    }

    for reg_method in ["register_search", "register_fetch"] {
        let spec = web.methods.get(reg_method).unwrap_or_else(|| panic!("web.{reg_method} missing from contract"));
        assert_eq!(spec.expose, mare_kernel::contracts::Expose::Both);
        assert_eq!(spec.access, mare_kernel::contracts::Access::Peer);
        assert!(spec.registration);
        assert!(!spec.remote_callbacks.is_empty(), "web.{reg_method} must declare callback schemas");
    }

    let derived_methods = derived();
    for m in ["web.list_providers", "web.search", "web.fetch", "web.register_search", "web.register_fetch"] {
        assert!(derived_methods.contains_key(m), "{m} must be derived from contracts");
    }
    for old in ["web.providers", "web.selection", "web.register", "web.unregister"] {
        assert!(!derived_methods.contains_key(old), "{old} must not exist in derived contracts");
    }

    let ui = all.get("ui").expect("ui missing from service contracts");
    for ui_reg in ["register_entry_renderer", "register_markdown_transform"] {
        let spec = ui.methods.get(ui_reg).unwrap_or_else(|| panic!("ui.{ui_reg} missing from contract"));
        assert_eq!(spec.expose, mare_kernel::contracts::Expose::Both);
        assert_eq!(spec.access, mare_kernel::contracts::Access::Peer);
        assert!(spec.registration);
        assert!(!spec.remote_callbacks.is_empty(), "ui.{ui_reg} must declare callback schemas");
        assert!(derived_methods.contains_key(&format!("ui.{ui_reg}")));
    }

    let proto_schema = mare_protocol::schema();
    let proto_methods = proto_schema["methods"].as_object().unwrap();
    let proto_callbacks = proto_schema["callbacks"].as_object().unwrap();
    for removed in [
        "web.providers",
        "web.selection",
        "web.search",
        "web.fetch",
        "web.register",
        "web.unregister",
        "entry.renderer",
        "markdown.register_transform",
    ] {
        assert!(!proto_methods.contains_key(removed), "{removed} must leave mare-protocol schema");
    }
    for removed_cb in ["web.call", "entry.render", "markdown.transform_call"] {
        assert!(!proto_callbacks.contains_key(removed_cb), "{removed_cb} must leave mare-protocol callbacks");
    }
}

#[test]
fn message_contract_is_full_and_derived() {
    let all = contracts();
    let msg = all.get("message").expect("message contract missing");
    assert!(!msg.methods.is_empty(), "message must have a full contract, not name-only");
    assert!(!msg.methods.contains_key("types"), "legacy message.types must be renamed to message.list_types");
    assert!(!msg.methods.contains_key("unregister_type"), "by-name unregister_type must be removed");

    let list_types = msg.methods.get("list_types").expect("message.list_types missing");
    assert_eq!(list_types.expose, mare_kernel::contracts::Expose::Both);
    assert_eq!(list_types.access, mare_kernel::contracts::Access::Peer);
    assert_eq!(list_types.scope, mare_kernel::contracts::Scope::Host);

    let append = msg.methods.get("append").expect("message.append missing");
    assert_eq!(append.expose, mare_kernel::contracts::Expose::Both);
    assert_eq!(append.access, mare_kernel::contracts::Access::Peer);
    assert_eq!(append.scope, mare_kernel::contracts::Scope::Session);

    let reg = msg.methods.get("register_type").expect("message.register_type missing");
    assert_eq!(reg.expose, mare_kernel::contracts::Expose::Both);
    assert_eq!(reg.access, mare_kernel::contracts::Access::Peer);
    assert_eq!(reg.scope, mare_kernel::contracts::Scope::Host);
    assert!(reg.registration);
    assert!(reg.remote_callbacks.contains_key("validate"));
    assert!(reg.remote_callbacks.contains_key("render"));

    let get_type = msg.methods.get("get_type").expect("message.get_type missing");
    assert_eq!(get_type.expose, mare_kernel::contracts::Expose::Lua);

    let derived_methods = derived();
    for name in ["message.list_types", "message.append", "message.register_type"] {
        assert!(derived_methods.contains_key(name), "{name} must be derived from contracts");
    }
    assert!(!derived_methods.contains_key("message.types"), "legacy message.types must not exist");

    let proto_schema = mare_protocol::schema();
    let proto_methods = proto_schema["methods"].as_object().unwrap();
    assert!(!proto_methods.keys().any(|k| k.starts_with("message.")), "message.* must leave mare-protocol schema");
}

#[tokio::test]
async fn proc_contract_is_full_and_lua_only() {
    let contracts = contracts();
    let contract = contracts.get("proc").expect("`proc` contract must be registered");
    assert!(!contract.methods.is_empty(), "`proc` must have a full contract, not name-only");
    let names: BTreeSet<&str> = contract.methods.keys().map(String::as_str).collect();
    assert_eq!(names, BTreeSet::from(["kill", "resize", "spawn", "write"]), "proc methods mismatch");
    for (name, spec) in &contract.methods {
        assert_eq!(
            spec.expose,
            mare_kernel::contracts::Expose::Lua,
            "proc.{name} must default to expose = \"lua\" (never remote)"
        );
        assert!(!spec.remote(), "proc.{name} must not be exposed to remote RPC");
    }
    let derived = derived();
    for m in [
        "proc.spawn",
        "proc.write",
        "proc.resize",
        "proc.kill",
        "process.spawn",
        "process.write",
        "process.resize",
        "process.kill",
    ] {
        assert!(!derived.contains_key(m), "`{m}` must not be derived for RPC");
    }
    local(async {
        let env = TestEnv::new().await;
        let host = env.host().builtins(&["proc"]).faux(json!([])).build().await;
        assert!(host.kernel.service("proc").is_some(), "built-in `proc` service must be provided");
        host.shutdown().await;
    })
    .await;
}

fn walk_schema_properties(schema: &Value, f: &mut impl FnMut(&str, &Value)) {
    let Some(obj) = schema.as_object() else { return };
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for (name, prop_schema) in props {
            f(name, prop_schema);
            walk_schema_properties(prop_schema, f);
        }
    }
    if let Some(items) = obj.get("items") {
        walk_schema_properties(items, f);
    }
    if let Some(addl) = obj.get("additionalProperties") {
        walk_schema_properties(addl, f);
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(arr) = obj.get(key).and_then(Value::as_array) {
            for item in arr {
                walk_schema_properties(item, f);
            }
        }
    }
}

fn walk_json_keys(value: &Value, f: &mut impl FnMut(&str, &Value)) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                f(k, v);
                walk_json_keys(v, f);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                walk_json_keys(item, f);
            }
        }
        _ => {}
    }
}

fn is_schema_type(schema: &Value, expected: &str) -> bool {
    match schema.get("type") {
        Some(Value::String(s)) => s == expected,
        Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(expected)),
        _ => false,
    }
}

fn check_exempt_set(kind: &str, observed: BTreeSet<String>, exempt: &[&str], problems: &mut Vec<String>) {
    let exempt_set: BTreeSet<String> = exempt.iter().map(|s| (*s).to_owned()).collect();
    for item in observed.difference(&exempt_set) {
        problems.push(format!("[{kind}] unexempted violation: `{item}`"));
    }
    for stale in exempt_set.difference(&observed) {
        problems.push(format!("[{kind}] stale exemption (violation already removed): `{stale}`"));
    }
}

/// V-batch invariants:
/// Every legacy exemption list is strictly shrink-only — removing a violation requires
/// removing its exemption in the same change, and no new violation can enter.
#[test]
fn contract_vocabulary_and_shape_invariants() {
    use mare_kernel::contracts::Scope;

    const BANNED_BOOL_RESULT_PROPS: &[&str] = &["ok", "cancelled", "killed", "delivered", "removed", "stopped"];
    const WORK_STATUS_VOCAB: &[&str] = &["running", "completed", "failed", "cancelled", "interrupted"];
    const COMPACT_RUN_RESULTS: &[&str] = &["compacted", "nothing_to_compact", "unchanged", "summarized"];
    const BANNED_METHOD_NAMES: &[&str] = &[
        "spawn",
        "open",
        "kill",
        "abort",
        "abort_turn",
        "halt",
        "remove",
        "queue_remove",
        "remove_owned",
        "forget",
        "terminate",
        "override",
        "markdown_document",
        "markdown_snapshot",
        "markdown_cancel",
        "markdown_forget",
        "open_transcript",
    ];
    // Verbs permitted on remote methods (canonical API naming + current domain operations).
    const ALLOWED_REMOTE_VERBS: &[&str] = &[
        "list",
        "get",
        "search",
        "read",
        "create",
        "load",
        "start",
        "wait",
        "update",
        "set",
        "cancel",
        "close",
        "delete",
        "install",
        "uninstall",
        "register",
        "run",
        "watch",
        "subscribe",
        "render",
        "invalidate",
        "reset",
        "background",
        "send",
        "signal",
        "resize",
        "steer",
        "prompt",
        "continue",
        "save",
        "write",
        "stat",
        "append",
        "login",
        "logout",
        "connect",
        "reconnect",
        "list_servers",
        "list_tools",
        "list_resources",
        "list_prompts",
        "read_resource",
        "get_prompt",
        "list_models",
        "list_providers",
        "get_model",
        "set_model",
        "set_effort",
        "list_types",
        "list_backends",
        "list_markdown_presenters",
        "transform_markdown",
        "select",
        "preview",
        "cancel_preview",
        "fetch",
        "archive",
        "unarchive",
        "label_item",
        "call",
    ];
    const EXPECTED_CONFINED: &[&str] = &[
        "fs.list",
        "fs.read",
        "fs.stat",
        "fs.watch",
        "fs.write",
        "plugins.check",
        "session.get_item",
        "session.search",
        "session.search_items",
        "session.trace",
        "session.trace_item",
        "web.fetch",
        "web.search",
    ];

    // --- Shrink-only exemption lists (cleared per migrated domain) ---
    const EXEMPT_BOOL_RESULT: &[&str] = &[];
    const EXEMPT_NEXT_CURSOR: &[&str] = &[];
    const EXEMPT_LIST_DATA: &[&str] = &[];
    const EXEMPT_ID_OR_HANDLE: &[&str] = &[];
    const EXEMPT_NON_STRING_CURSOR: &[&str] = &[];
    const EXEMPT_METHOD_VERB: &[&str] = &[];
    const EXEMPT_STATUS_ENUM: &[&str] = &[];
    const EXEMPT_CORE_ERROR_COLLISION: &[&str] = &[];
    const EXEMPT_REMOTE_OPTIONAL: &[&str] = &[];
    const EXEMPT_SESSION_SCOPE: &[&str] = &[];
    const EXEMPT_LOCAL_ACCESS: &[&str] = &[];
    const EXEMPT_GENERATION: &[&str] = &[];
    const EXEMPT_TIMESTAMP: &[&str] = &[];

    // Domain-specific remote verbs accepted for their owning service.
    const DOMAIN_REMOTE_VERBS: &[(&str, &[&str])] =
        &[("session", &["fork", "rewind", "snapshot", "trace", "trace_item", "add_record"])];
    // Session lifecycle status enum accepted alongside work statuses.
    const SESSION_STATUS_VOCAB: &[&str] = &["idle", "busy"];

    fn is_numeric_timestamp_schema(schema: &Value) -> bool {
        if is_schema_type(schema, "integer") || is_schema_type(schema, "number") {
            return true;
        }
        schema.get("oneOf").and_then(Value::as_array).is_some_and(|variants| {
            variants.iter().any(|v| is_schema_type(v, "integer") || is_schema_type(v, "number"))
        })
    }

    fn is_non_ms_timestamp_prop(prop: &str) -> bool {
        prop == "at"
            || prop.ends_with("_at")
            || prop == "time"
            || prop.ends_with("_time")
            || prop == "timestamp"
            || prop.ends_with("_timestamp")
            || prop == "created"
            || prop.ends_with("_created")
            || prop == "updated"
            || prop.ends_with("_updated")
            || prop == "started"
            || prop == "ended"
            || prop == "expires"
            || prop.ends_with("_expires")
            || (prop.starts_with("created_at_") && prop != "created_at_ms")
            || (prop.starts_with("updated_at_") && prop != "updated_at_ms")
            || matches!(prop, "time_from" | "time_to")
    }

    const ALLOWED_HYPHEN_ENUMS: &[&str] = &[
        "read-only",
        "full-access",
        "workspace-write",
        "danger-full-access",
        "assistant-reasoning",
        "above-editor",
        "below-editor",
    ];

    let mut obs_bool_result = BTreeSet::new();
    let mut obs_next_cursor = BTreeSet::new();
    let mut obs_list_data = BTreeSet::new();
    let mut obs_id_handle = BTreeSet::new();
    let mut obs_non_str_cursor = BTreeSet::new();
    let mut obs_verb = BTreeSet::new();
    let mut obs_status_enum = BTreeSet::new();
    let mut obs_hyphen_enum = BTreeSet::new();
    let mut obs_core_err = BTreeSet::new();
    let mut obs_remote_opt = BTreeSet::new();
    let mut obs_session_scope = BTreeSet::new();
    let mut obs_local_access = BTreeSet::new();
    let mut obs_confined = BTreeSet::new();
    let mut obs_generation = BTreeSet::new();
    let mut obs_timestamp = BTreeSet::new();

    for (service, contract) in contracts() {
        for err_name in contract.errors.keys() {
            if mare_shared::errors::is_core(err_name) {
                obs_core_err.insert(format!("{service}.{err_name}"));
            }
        }
        for (method, spec) in &contract.methods {
            let fq = format!("{service}.{method}");

            if spec.confined {
                obs_confined.insert(fq.clone());
            }
            if !spec.remote() && spec.access_declared {
                obs_local_access.insert(fq.clone());
            }
            if spec.remote() && spec.optional {
                obs_remote_opt.insert(fq.clone());
            }

            // Wire-shape vocabulary rules (Rules 1-4) govern remote RPC methods.
            if spec.remote() {
                // Rule 1: forbidden boolean result fields
                let mut has_bad_bool = false;
                walk_schema_properties(&spec.result, &mut |prop, prop_schema| {
                    if BANNED_BOOL_RESULT_PROPS.contains(&prop) && is_schema_type(prop_schema, "boolean") {
                        has_bad_bool = true;
                    }
                });
                if has_bad_bool {
                    obs_bool_result.insert(fq.clone());
                }

                // Rule 2: pagination next_cursor iff cursor+limit; list*/search* must have array data
                let result_props = spec.result.get("properties").and_then(Value::as_object);
                let params_props = spec.params.get("properties").and_then(Value::as_object);
                let has_next_cursor = result_props.is_some_and(|p| p.contains_key("next_cursor"));
                let has_cursor_and_limit =
                    params_props.is_some_and(|p| p.contains_key("cursor") && p.contains_key("limit"));
                if has_next_cursor != has_cursor_and_limit && fq != "query.call" {
                    obs_next_cursor.insert(fq.clone());
                }
                if method == "list" || method.starts_with("list_") || method.starts_with("search") {
                    let has_data_arr =
                        result_props.and_then(|p| p.get("data")).is_some_and(|d| is_schema_type(d, "array"));
                    if !has_data_arr {
                        obs_list_data.insert(fq.clone());
                    }
                }
            }

            // Vocabulary rules (Rules 3, 8, 9, and snake_case enums) govern BOTH remote and Lua-only contracts.
            let mut has_id_handle = false;
            let mut has_non_str_cursor = false;
            for root in [&spec.params, &spec.result] {
                walk_schema_properties(root, &mut |prop, prop_schema| {
                    if matches!(prop, "id" | "handle") {
                        has_id_handle = true;
                    }
                    if spec.remote()
                        && matches!(prop, "cursor" | "next_cursor")
                        && !is_schema_type(prop_schema, "string")
                    {
                        has_non_str_cursor = true;
                    }
                    if prop.contains("generation") {
                        obs_generation.insert(format!("{fq}.{prop}"));
                    }
                    if is_non_ms_timestamp_prop(prop)
                        || (prop.ends_with("_at_ms") && !is_numeric_timestamp_schema(prop_schema))
                    {
                        obs_timestamp.insert(format!("{fq}.{prop}"));
                    }
                    if let Some(vals) = prop_schema.get("enum").and_then(Value::as_array) {
                        for v in vals {
                            if let Some(s) = v.as_str()
                                && s.contains('-')
                                && !ALLOWED_HYPHEN_ENUMS.contains(&s)
                            {
                                obs_hyphen_enum.insert(format!("{fq}.{prop}={s}"));
                            }
                        }
                    }
                    if let Some(vals) = prop_schema.pointer("/items/enum").and_then(Value::as_array) {
                        for v in vals {
                            if let Some(s) = v.as_str()
                                && s.contains('-')
                                && !ALLOWED_HYPHEN_ENUMS.contains(&s)
                            {
                                obs_hyphen_enum.insert(format!("{fq}.{prop}[]={s}"));
                            }
                        }
                    }
                });
            }
            if has_id_handle {
                obs_id_handle.insert(fq.clone());
            }
            if has_non_str_cursor {
                obs_non_str_cursor.insert(fq.clone());
            }

            // Rule 4: verbs and status enums
            let proc_exception = service == "proc" && matches!(method.as_str(), "spawn" | "kill");
            let plugin_check = fq == "plugins.check";
            let domain_verb = DOMAIN_REMOTE_VERBS
                .iter()
                .find(|(svc, _)| *svc == service.as_str())
                .is_some_and(|(_, verbs)| verbs.contains(&method.as_str()));
            if !proc_exception && !plugin_check {
                if BANNED_METHOD_NAMES.contains(&method.as_str()) {
                    obs_verb.insert(fq.clone());
                } else if spec.remote()
                    && !domain_verb
                    && !ALLOWED_REMOTE_VERBS.contains(&method.as_str())
                    && !method.split('_').next().is_some_and(|v| ALLOWED_REMOTE_VERBS.contains(&v))
                {
                    obs_verb.insert(fq.clone());
                }
            }
            const ACCEPTED_DELIVERY_STATUS: &[&str] = &["appended", "queued", "deferred", "triggered"];
            let mut bad_status = false;
            if spec.remote() {
                if plugin_check {
                    assert!(spec.remote() && spec.confined, "plugins.check fetches through the confined boundary");
                    assert_eq!(
                        spec.result["properties"]["data"]["items"]["properties"]["status"]["enum"],
                        json!(["up_to_date", "outdated", "pinned", "local", "unreachable"]),
                        "plugin revision statuses are distinct from work statuses"
                    );
                } else if fq != "agent.prompt" {
                    let result_statuses = if fq == "compact.run" { COMPACT_RUN_RESULTS } else { WORK_STATUS_VOCAB };
                    for (root, allowed) in [(&spec.params, WORK_STATUS_VOCAB), (&spec.result, result_statuses)] {
                        walk_schema_properties(root, &mut |prop, prop_schema| {
                            if prop == "status"
                                && let Some(vals) = prop_schema.get("enum").and_then(Value::as_array)
                                && vals.iter().any(|v| {
                                    !v.as_str().is_some_and(|s| {
                                        allowed.contains(&s)
                                            || (service == "session" && SESSION_STATUS_VOCAB.contains(&s))
                                            || (fq == "message.append" && ACCEPTED_DELIVERY_STATUS.contains(&s))
                                    })
                                })
                            {
                                bad_status = true;
                            }
                        });
                    }
                } else {
                    // explicitly specifies `agent.prompt` acceptance statuses `started | queued`.
                    let st_vals = spec.result["properties"]["status"]["enum"].as_array().unwrap();
                    assert_eq!(st_vals, &vec![json!("started"), json!("queued")]);
                }
            }
            if bad_status {
                obs_status_enum.insert(fq.clone());
            }

            // Rule 7: session-scoped remote methods must declare `scope = "session"`
            let must_be_session = spec.remote()
                && (matches!(service.as_str(), "agent" | "cells" | "jobs" | "terminal" | "subagent" | "permissions")
                    || (service == "spill" && matches!(method.as_str(), "save" | "read" | "delete"))
                    || (service == "message" && method == "append")
                    || (service == "usage" && method == "get")
                    || (service == "compact" && method == "run")
                    || (service == "llm" && matches!(method.as_str(), "get_model" | "set_model" | "set_effort"))
                    || (service == "ui"
                        && matches!(
                            method.as_str(),
                            "render_markdown" | "get_markdown" | "cancel_markdown" | "delete_markdown"
                        ))
                    || (service == "session" && matches!(method.as_str(), "update" | "label_item")));
            if must_be_session && spec.scope != Scope::Session {
                obs_session_scope.insert(fq);
            }
        }

        // Expand Rules 3, 8, and 9 to every contract event's `data` schema (including open objects) and `example`.
        for ev_spec in contract.events.values() {
            let ev_name = &ev_spec.name;
            walk_schema_properties(&ev_spec.data, &mut |prop, prop_schema| {
                if matches!(prop, "id" | "handle") {
                    obs_id_handle.insert(format!("event:{ev_name}.{prop}"));
                }
                if prop.contains("generation") {
                    obs_generation.insert(format!("event:{ev_name}.{prop}"));
                }
                if is_non_ms_timestamp_prop(prop)
                    || (prop.ends_with("_at_ms") && !is_numeric_timestamp_schema(prop_schema))
                {
                    obs_timestamp.insert(format!("event:{ev_name}.{prop}"));
                }
            });
            if let Some(example) = &ev_spec.example {
                walk_json_keys(example, &mut |prop, val| {
                    if matches!(prop, "id" | "handle") {
                        obs_id_handle.insert(format!("event:{ev_name}.example.{prop}"));
                    }
                    if prop.contains("generation") {
                        obs_generation.insert(format!("event:{ev_name}.example.{prop}"));
                    }
                    if is_non_ms_timestamp_prop(prop)
                        || (prop.ends_with("_at_ms") && !val.is_number() && !val.is_null())
                    {
                        obs_timestamp.insert(format!("event:{ev_name}.example.{prop}"));
                    }
                });
            }
        }
    }

    let mut problems = vec![];
    check_exempt_set("bool-result", obs_bool_result, EXEMPT_BOOL_RESULT, &mut problems);
    check_exempt_set("next-cursor", obs_next_cursor, EXEMPT_NEXT_CURSOR, &mut problems);
    check_exempt_set("list-data", obs_list_data, EXEMPT_LIST_DATA, &mut problems);
    check_exempt_set("id-or-handle", obs_id_handle, EXEMPT_ID_OR_HANDLE, &mut problems);
    check_exempt_set("non-string-cursor", obs_non_str_cursor, EXEMPT_NON_STRING_CURSOR, &mut problems);
    check_exempt_set("method-verb", obs_verb, EXEMPT_METHOD_VERB, &mut problems);
    check_exempt_set("status-enum", obs_status_enum, EXEMPT_STATUS_ENUM, &mut problems);
    check_exempt_set("hyphen-enum", obs_hyphen_enum, &[], &mut problems);
    check_exempt_set("core-error-collision", obs_core_err, EXEMPT_CORE_ERROR_COLLISION, &mut problems);
    check_exempt_set("remote-optional", obs_remote_opt, EXEMPT_REMOTE_OPTIONAL, &mut problems);
    check_exempt_set("session-scope", obs_session_scope, EXEMPT_SESSION_SCOPE, &mut problems);
    check_exempt_set("local-access", obs_local_access, EXEMPT_LOCAL_ACCESS, &mut problems);
    check_exempt_set("confined-snapshot", obs_confined, EXPECTED_CONFINED, &mut problems);
    check_exempt_set("generation", obs_generation, EXEMPT_GENERATION, &mut problems);
    check_exempt_set("timestamp", obs_timestamp, EXEMPT_TIMESTAMP, &mut problems);

    assert!(problems.is_empty(), "API vocabulary/shape assertion failures:\n{}", problems.join("\n"));
}

/// Every declared contract event uses `root/name`, every `*-started`
/// event pairs with a same-root `*-completed` event, and forbidden legacy suffixes (`-ended`,
/// `-complete`, bare `/change`) are rejected.
#[test]
fn event_names_are_rooted_and_past_tense_pairs() {
    let mut all_events = BTreeSet::new();
    for contract in contracts().values() {
        for ev_spec in contract.events.values() {
            all_events.insert(ev_spec.name.clone());
        }
    }
    let mut problems = Vec::new();
    for name in &all_events {
        if !name.contains('/') {
            problems.push(format!("event `{name}` is missing a `root/` prefix"));
            continue;
        }
        if name.ends_with("-ended") || name.ends_with("/ended") {
            problems.push(format!(
                "event `{name}` uses forbidden `-ended`/`/ended` suffix (use `-completed` or `/closed`)"
            ));
        }
        if name.ends_with("-complete") {
            problems.push(format!("event `{name}` uses forbidden `-complete` suffix (use `-completed`)"));
        }
        if name.ends_with("/change") {
            problems.push(format!("event `{name}` uses bare `/change` (use past-tense `/changed`)"));
        }
        if let Some(stem) = name.strip_suffix("-started") {
            let pair = format!("{stem}-completed");
            if !all_events.contains(&pair) {
                problems.push(format!("event `{name}` has no matching `{pair}` event"));
            }
        }
    }
    assert!(problems.is_empty(), "event naming invariant failures:\n{}", problems.join("\n"));
}

#[test]
fn access_unification_invariants() {
    use mare_kernel::contracts::Access;
    let all = contracts();

    let cells = all.get("cells").expect("cells contract missing");
    for (m, expected) in [
        ("start", Access::Primary),
        ("wait", Access::Primary),
        ("cancel", Access::Primary),
        ("reset", Access::Primary),
        ("list", Access::Peer),
        ("get", Access::Peer),
        ("list_tools", Access::Peer),
    ] {
        assert_eq!(cells.methods[m].access, expected, "cells.{m} access mismatch");
    }

    let hooks = all.get("hooks").expect("hooks contract missing");
    assert_eq!(hooks.methods["subscribe"].access, Access::Primary, "hooks.subscribe must be primary");

    let proto = mare_protocol::schema();
    let proto_methods = proto["methods"].as_object().unwrap();
    for primary_method in ["services.register", "options.set", "state.compare_exchange"] {
        assert_eq!(
            proto_methods[primary_method]["access"].as_str(),
            Some("primary"),
            "{primary_method} must be primary"
        );
    }

    let llm = all.get("llm").expect("llm contract missing");
    let list_auth_keys = llm.methods.get("list_auth_keys").expect("llm.list_auth_keys missing");
    assert_eq!(list_auth_keys.expose, mare_kernel::contracts::Expose::Both, "llm.list_auth_keys must be expose=both");
    assert_eq!(list_auth_keys.access, Access::Primary, "llm.list_auth_keys must be primary");
    assert!(list_auth_keys.admin, "llm.list_auth_keys must require admin");

    let session = all.get("session").expect("session contract missing");
    for spi in [
        "persist_message",
        "record_tool_state",
        "record_request",
        "record_request_end",
        "record_request_aborted",
        "record_run_end",
    ] {
        let props = session.methods[spi].params["properties"].as_object().expect("spi params properties");
        assert!(props.contains_key("skip_quarantine"), "session.{spi} must declare skip_quarantine in contract");
    }
    assert!(
        session.methods["persist_message"].params["properties"]
            .as_object()
            .unwrap()
            .contains_key("skip_initial_model_quarantine"),
        "session.persist_message must declare skip_initial_model_quarantine in contract"
    );

    let ui = all.get("ui").expect("ui contract missing");
    let status_def = ui.methods["status"].callbacks.get("def").map(String::as_str).unwrap_or("");
    assert!(
        status_def.contains("flush_gen?: integer"),
        "ui.status contract must document flush_gen?: integer: {status_def}"
    );

    let tools = all.get("tools").expect("tools contract missing");
    let register_callbacks = &tools.methods["register"].callbacks;
    assert!(
        !register_callbacks.contains_key("defer") && !register_callbacks.contains_key("defer_loading"),
        "tools.register must use exposure=\"deferred\" instead of legacy defer/defer_loading keys"
    );
}

#[test]
fn agent_contract_is_full_and_derived() {
    use mare_kernel::contracts::{Access, Expose, Scope};

    let all = contracts();
    let agent = all.get("agent").expect("agent contract must exist");
    assert!(agent.owner.is_none(), "agent must have a full contract");

    for name in [
        "prompt",
        "steer",
        "cancel",
        "continue",
        "wait",
        "get",
        "list_queue",
        "update_queued",
        "delete_queued",
        "list_messages",
    ] {
        let method = agent.methods.get(name).unwrap_or_else(|| panic!("agent.{name} missing from contract"));
        assert_eq!(method.expose, Expose::Both, "agent.{name} must be expose=both");
        assert_eq!(method.access, Access::Peer, "agent.{name} access mismatch");
        assert_eq!(method.scope, Scope::Session, "agent.{name} must be scope=session");
    }

    let messages_method = agent.methods.get("messages").expect("agent.messages missing from contract");
    assert_eq!(messages_method.expose, Expose::Lua, "agent.messages must be expose=lua");
    assert!(!messages_method.access_declared, "agent.messages must not declare access");

    for read_or_cancel in ["cancel", "get", "list_queue", "list_messages", "wait"] {
        assert_eq!(
            agent.methods[read_or_cancel].poisoned,
            Some(true),
            "agent.{read_or_cancel} must allow poisoned sessions"
        );
    }

    for removed in [
        "submit",
        "steer_input",
        "resume",
        "rewrite_queued",
        "abort",
        "abort_turn",
        "follow_up",
        "try_prompt",
        "queued",
        "queue_list",
        "queue_remove",
        "queue_clear",
        "edit_queued",
        "move_queued",
        "state",
        "busy",
        "prompt_internal",
        "steer_internal",
        "continue_internal",
        "update_queued_internal",
        "list_messages_internal",
    ] {
        assert!(!agent.methods.contains_key(removed), "legacy agent.{removed} must be removed");
    }

    let derived_methods = derived();
    for name in [
        "agent.prompt",
        "agent.steer",
        "agent.cancel",
        "agent.continue",
        "agent.wait",
        "agent.get",
        "agent.list_queue",
        "agent.update_queued",
        "agent.delete_queued",
        "agent.list_messages",
    ] {
        assert!(derived_methods.contains_key(name), "{name} must be derived");
    }
    for removed in ["agent.abort", "agent.follow_up", "agent.try_prompt"] {
        assert!(!derived_methods.contains_key(removed), "{removed} must not be derived");
    }

    let proto_schema = mare_protocol::schema();
    let proto_methods = proto_schema["methods"].as_object().unwrap();
    for legacy in ["prompt", "prompt.try", "steer", "follow_up", "abort"] {
        assert!(!proto_methods.contains_key(legacy), "{legacy} must leave mare-protocol schema");
    }
}

#[test]
fn every_contract_method_is_typed_and_documented() {
    let lua = mlua::Lua::new();
    let raw: mlua::Table = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();

    let mut name_only_services = Vec::new();
    let mut name_only_methods = 0usize;
    let mut missing_schema = Vec::new();
    let mut missing_result = Vec::new();
    let mut missing_doc = Vec::new();

    for pair in raw.pairs::<String, mlua::Table>() {
        let (service, contract) = pair.unwrap();
        if contract.contains_key("owner").unwrap() && !contract.contains_key("methods").unwrap() {
            continue;
        }
        let Ok(methods) = contract.get::<mlua::Table>("methods") else {
            let req = contract.get::<Option<mlua::Table>>("required").unwrap().map_or(0, |t| t.raw_len());
            let opt = contract.get::<Option<mlua::Table>>("optional").unwrap().map_or(0, |t| t.raw_len());
            name_only_methods += req + opt;
            name_only_services.push(service);
            continue;
        };
        for row in methods.pairs::<String, mlua::Table>() {
            let (method, spec) = row.unwrap();
            let fq = format!("{service}.{method}");
            let has_params = spec.contains_key("params").unwrap();
            let has_result = spec.contains_key("result").unwrap();
            let is_reg = spec.get::<Option<bool>>("registration").unwrap().unwrap_or(false);
            let has_lua = spec.contains_key("lua").unwrap();
            let has_cb = spec.contains_key("callbacks").unwrap();
            let expose = spec.get::<Option<String>>("expose").unwrap().unwrap_or_else(|| "lua".into());
            let is_remote = expose == "both";
            if !is_remote && !has_params && !has_result && !is_reg && !has_lua && !has_cb {
                missing_schema.push(fq.clone());
            }
            if !is_reg && !has_result {
                missing_result.push(fq.clone());
            }
            let doc = spec.get::<Option<String>>("doc").unwrap().unwrap_or_default();
            if doc.trim().is_empty() {
                missing_doc.push(fq);
            }
        }
    }

    name_only_services.sort();
    missing_schema.sort();
    missing_result.sort();
    missing_doc.sort();

    let services_dir = preset().join("types/mare/services");
    let names_lua_exists = services_dir.join("contracts/names.lua").exists();
    let legacy_handwritten_files: Vec<&str> =
        ["agent.lua", "session.lua", "llm.lua"].into_iter().filter(|file| services_dir.join(file).exists()).collect();
    let method_re = regex::Regex::new(r"(?m)^function (\w+):(\w+)\(").unwrap();
    let mut handwritten_service_methods = Vec::new();
    for entry in std::fs::read_dir(&services_dir).unwrap() {
        let path = entry.unwrap().path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.ends_with(".gen.lua") || name == "ctx.lua" {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for cap in method_re.captures_iter(&text) {
            handwritten_service_methods.push(format!("{name}:{}:{}", &cap[1], &cap[2]));
        }
    }
    handwritten_service_methods.sort();

    assert!(
        name_only_services.is_empty()
            && !names_lua_exists
            && missing_schema.is_empty()
            && missing_result.is_empty()
            && missing_doc.is_empty()
            && legacy_handwritten_files.is_empty()
            && handwritten_service_methods.is_empty(),
        "typed contract parity failed:\n\
         - name_only_services ({}, {} methods): {:?}\n\
         - names.lua exists: {}\n\
         - missing_schema ({}): {:?}\n\
         - missing_result ({}): {:?}\n\
         - missing_doc ({}): {:?}\n\
         - legacy hand-written files: {:?}\n\
         - hand-written service methods ({}): {:?}",
        name_only_services.len(),
        name_only_methods,
        name_only_services,
        names_lua_exists,
        missing_schema.len(),
        &missing_schema[..missing_schema.len().min(10)],
        missing_result.len(),
        &missing_result[..missing_result.len().min(10)],
        missing_doc.len(),
        &missing_doc[..missing_doc.len().min(10)],
        legacy_handwritten_files,
        handwritten_service_methods.len(),
        &handwritten_service_methods[..handwritten_service_methods.len().min(10)],
    );
}

#[tokio::test]
async fn approvals_gate_allows_and_denies_via_service_proxy() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(json!({
                "model": "faux/echo",
                "plugins": { "approvals": { "policy": "ask" } }
            }))
            .build()
            .await;
        global(&h, "H_APPROVALS", h.service_table("approvals").unwrap());
        let (ask_allowed, never_decision, gran_allowed, gran_denied): (Option<String>, String, Option<String>, String) =
            eval(
                &h,
                r#"
                local a = assert(H_APPROVALS)
                local ask_res = a:gate("shell")
                a:set("never")
                local never_res = a:gate("shell")
                a:set({ mode = "granular", categories = { shell = true, net = false } })
                local gran_ok = a:gate("shell")
                local gran_no = a:gate("net")
                return ask_res and ask_res.decision or nil,
                    assert(never_res).decision,
                    gran_ok and gran_ok.decision or nil,
                    assert(gran_no).decision
                "#,
            )
            .await;
        assert_eq!(ask_allowed, None, "ask policy must allow shell and return nil");
        assert_eq!(never_decision, "deny", "never policy must deny shell");
        assert_eq!(gran_allowed, None, "granular allowed category must return nil");
        assert_eq!(gran_denied, "deny", "granular disallowed category must deny");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn llm_model_sources_accepts_model_table_and_changes_rewound_turn_is_string() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "ok" }])).build().await;
        global(&h, "H_LLM", h.service_table("llm").unwrap());
        let ok: bool = eval(
            &h,
            r#"
            local m = assert(H_LLM:current())
            local srcs = H_LLM:model_sources(m)
            return srcs == nil or type(srcs) == "table"
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;

        let lua = mlua::Lua::new();
        let contracts_val: mlua::Value = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
        let events = mare_kernel::contracts::rpc_events(&lua, contracts_val).unwrap();
        let rewound = events.get("changes/rewound").expect("changes/rewound event contract");
        assert_eq!(rewound["data_schema"]["properties"]["turn"]["type"], json!("string"), "{rewound}");
    })
    .await;
}

#[test]
fn kernel_contract_is_host_provided_and_lua_only() {
    use mare_kernel::contracts::{EventReach, Expose};
    let all = contracts();
    let kernel = all.get("kernel").expect("kernel contract missing");
    assert!(kernel.owner.is_none(), "kernel must be a full service contract");
    assert!(!kernel.methods.is_empty(), "kernel contract must declare methods");
    for (name, spec) in &kernel.methods {
        assert!(spec.is_host(), "kernel.{name} must have provider = host");
        assert_eq!(spec.expose, Expose::Lua, "kernel.{name} must have expose = lua");
    }
    assert_eq!(kernel.events["before-reload"].expose, EventReach::Process);
    assert_eq!(kernel.events["doctor"].expose, EventReach::Process);
    assert_eq!(kernel.events["doctor"].emitters, vec!["commands".to_string()]);

    let option = all.get("option").expect("option event-only contract missing");
    assert_eq!(option.owner.as_deref(), Some("kernel"));
    assert_eq!(option.events["apply"].expose, EventReach::Process);
    assert_eq!(option.events["current"].expose, EventReach::Process);

    let generated =
        std::fs::read_to_string(preset().join("types/mare/services/kernel.gen.lua")).expect("kernel.gen.lua missing");
    assert!(generated.contains("---@class mare.host.KernelService"), "{generated}");
}

#[test]
fn session_lifecycle_methods_are_host_provided_and_lua_only_helpers_stay_lua() {
    let all = contracts();
    let session = &all["session"];
    for method in ["create", "load", "fork", "close"] {
        let spec = session.methods.get(method).unwrap_or_else(|| panic!("session.{method} missing from contract"));
        assert_eq!(
            spec.provider,
            mare_kernel::contracts::MethodProvider::Host,
            "session.{method} must declare provider = 'host'"
        );
        assert!(spec.remote(), "session.{method} must stay remotely exposed");
    }
    assert!(!session.methods.contains_key("hold"), "session.hold must be removed from T/session.lua");
    for helper in ["switch", "prepare_fork"] {
        let spec = session.methods.get(helper).unwrap_or_else(|| panic!("session.{helper} missing from contract"));
        assert!(!spec.remote(), "session.{helper} must stay expose = 'lua' (not remote)");
        assert_eq!(
            spec.provider,
            mare_kernel::contracts::MethodProvider::Plugin,
            "session.{helper} is implemented by the session provider plugin"
        );
    }
    let kernel = &all["kernel"];
    assert!(
        !kernel.methods.contains_key("register_fork_prep"),
        "removed kernel.register_fork_prep must not remain in the kernel contract"
    );
    assert!(
        !kernel.methods.contains_key("take_fork_verified_images"),
        "removed kernel.take_fork_verified_images must not remain in the kernel contract"
    );
}

#[test]
fn admin_declared_set_matches_legacy_admin_policy_without_dead_names() {
    let lua = mlua::Lua::new();
    let contracts_val: mlua::Value = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
    let contracts = mare_kernel::contracts::parse_contracts(&lua, contracts_val).unwrap();

    let mut declared_contract_admin = BTreeSet::new();
    let mut declared_remote_admin = BTreeSet::new();
    for (svc, contract) in &contracts {
        for (method, spec) in &contract.methods {
            let desc = spec.describe();
            if desc.get("admin").and_then(Value::as_bool) == Some(true) {
                let full = format!("{svc}.{method}");
                declared_contract_admin.insert(full.clone());
                if spec.remote() {
                    declared_remote_admin.insert(full);
                }
            }
        }
    }

    let proto_schema = mare_protocol::schema();
    let mut declared_protocol_admin = BTreeSet::new();
    for (name, entry) in proto_schema["methods"].as_object().unwrap() {
        if entry.get("admin").and_then(Value::as_bool) == Some(true) {
            declared_protocol_admin.insert(name.clone());
            declared_remote_admin.insert(name.clone());
        }
    }

    let expected_protocol_admin: BTreeSet<String> =
        ["options.set", "services.register"].into_iter().map(str::to_owned).collect();
    assert_eq!(
        declared_protocol_admin, expected_protocol_admin,
        "protocol admin=true set must match expected protocol admin methods"
    );

    let mut expected_contract_admin: BTreeSet<String> = [
        "llm.list_auth_keys",
        "llm.login",
        "llm.logout",
        "llm.register_provider",
        "tools.register",
        "hooks.subscribe",
        "commands.run",
        "commands.register",
        "ui.send_editor",
        "ui.update_editor",
        "ui.call_keymap",
        "ui.set_external_editor",
        "ui.set_keymap",
        "ui.delete_keymap",
        "mcp.connect",
        "mcp.reconnect",
        "web.register_fetch",
        "web.register_search",
        "plugins.install",
        "plugins.update",
        "plugins.uninstall",
        "fs.write",
        "proc.spawn",
        "terminal.create",
        "cells.start",
        "subagent.register_type",
        "webserver.register_route",
        "webserver.register_assets",
        "workspace.set_trust",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for svc in ["permissions", "approvals"] {
        for method in contracts[svc].methods.keys() {
            // Read-only remote queries (`permissions.get`, `permissions.list`) match other read queries (non-admin).
            if svc == "permissions" && matches!(method.as_str(), "get" | "list") {
                continue;
            }
            expected_contract_admin.insert(format!("{svc}.{method}"));
        }
    }
    assert_eq!(
        declared_contract_admin, expected_contract_admin,
        "contract admin=true set must equal expected withheld list minus dead names \
         (process.spawn, sandbox.run, sandbox.start, cells.run) with permissions/approvals expanded"
    );

    // Every remote-exported admin=true method across contracts + protocol matches the exact live exported admin set.
    let mut expected_exported_admin: BTreeSet<String> = expected_protocol_admin.clone();
    for name in &expected_contract_admin {
        let (svc, meth) = name.split_once('.').unwrap();
        if contracts[svc].methods[meth].remote() {
            expected_exported_admin.insert(name.clone());
        }
    }
    assert_eq!(
        declared_remote_admin, expected_exported_admin,
        "exported remote admin=true set across contracts and protocol must match expected exported admin methods"
    );

    // Every remote session method (reads + writes) is grantable to peers by full name,
    // and the five session history/trace read methods are sandbox-confined.
    let confined_reads: BTreeSet<&str> =
        ["search", "search_items", "get_item", "trace", "trace_item"].into_iter().collect();
    for (meth, spec) in &contracts["session"].methods {
        if spec.remote() {
            assert_eq!(spec.access_name(), "peer", "remote session.{meth} must be access=peer");
            if confined_reads.contains(meth.as_str()) {
                assert!(spec.confined, "session.{meth} must be confined=true");
            }
        }
    }
}

#[test]
fn workspace_contract_is_host_provided_and_workspace_scoped() {
    use mare_kernel::contracts::{Access, Expose, MethodProvider, Scope};
    let proto_methods = mare_protocol::schema()["methods"].as_object().expect("protocol methods").clone();
    assert!(
        !proto_methods.contains_key("workspace.get") && !proto_methods.contains_key("workspace.set_trust"),
        "workspace.get / workspace.set_trust must move from mare-protocol to T/workspace.lua"
    );

    let all = contracts();
    assert!(!all.contains_key("baseline"), "baseline service contract must be deleted");

    let ws = all.get("workspace").expect("workspace contract missing");
    assert!(ws.owner.is_none(), "workspace must be a full service contract");
    assert_eq!(ws.methods.keys().cloned().collect::<Vec<_>>(), vec!["get", "set_trust"]);

    for (name, expected_access) in [("get", Access::Peer), ("set_trust", Access::Primary)] {
        let spec = &ws.methods[name];
        assert_eq!(spec.provider, MethodProvider::Host, "workspace.{name} must be provider = host");
        assert_eq!(spec.scope, Scope::Workspace, "workspace.{name} must be scope = workspace");
        assert_eq!(spec.expose, Expose::Both, "workspace.{name} must be expose = both");
        assert_eq!(spec.access, expected_access, "workspace.{name} access mismatch");
        assert!(
            spec.params.pointer("/properties/cwd").is_none(),
            "workspace.{name} must not declare cwd in contract params (RPC adapter adds it)"
        );
        assert!(
            spec.wire_params().pointer("/properties/cwd").is_some(),
            "workspace.{name} wire_params must automatically include cwd"
        );
    }

    let generated = std::fs::read_to_string(preset().join("types/mare/services/workspace.gen.lua"))
        .expect("workspace.gen.lua missing");
    assert!(generated.contains("---@class mare.WorkspaceService"), "{generated}");
}

/// Every JSON-RPC request example in `docs/{en,ja,zh}/integrate/rpc.md` must validate against the
/// live protocol or derived contract schema, and the retired `docs/rpc.md` must stay deleted.
#[test]
fn rpc_guide_json_examples_validate_against_schemas() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    assert!(!docs.join("rpc.md").exists(), "retired docs/rpc.md must not exist");

    let catalog = mare_protocol::schema();
    let proto_methods = catalog["methods"].as_object().expect("protocol methods");
    let contract_methods = derived();

    for lang in ["en", "ja", "zh"] {
        let path = docs.join(lang).join("integrate/rpc.md");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut seen = BTreeSet::new();
        let mut lines = text.lines();
        while let Some(line) = lines.next() {
            if line.trim() != "```json" {
                continue;
            }
            let mut block = String::new();
            for inner in lines.by_ref() {
                if inner.trim() == "```" {
                    break;
                }
                block.push_str(inner);
                block.push('\n');
            }
            let msg: Value = serde_json::from_str(&block)
                .unwrap_or_else(|e| panic!("{lang}: invalid json block in integrate/rpc.md: {e}\n{block}"));
            let Some(method) = msg.get("method").and_then(Value::as_str) else { continue };
            assert_eq!(msg.get("jsonrpc").and_then(Value::as_str), Some("2.0"), "{lang}: {method} missing jsonrpc 2.0");
            let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
            seen.insert(method.to_string());
            if proto_methods.contains_key(method) {
                let escaped = method.replace('~', "~0").replace('/', "~1");
                let ptr = format!("#/methods/{escaped}/params");
                mare_protocol::validate_schema_at(&catalog, &ptr, &params)
                    .unwrap_or_else(|e| panic!("{lang}: {method} example failed protocol schema: {e}\n{block}"));
            } else if let Some(entry) = contract_methods.get(method) {
                mare_protocol::validate_schema_at(&entry["params"], "#", &params)
                    .unwrap_or_else(|e| panic!("{lang}: {method} example failed contract schema: {e}\n{block}"));
            } else {
                panic!("{lang}: integrate/rpc.md JSON example names unknown method `{method}`");
            }
        }
        assert!(seen.contains("initialize"), "{lang}: integrate/rpc.md must include an initialize JSON example");
        assert!(seen.contains("tools.register"), "{lang}: integrate/rpc.md must include a tools.register JSON example");
    }

    // Negative check: the legacy `tools` array / `readonly` shape must be rejected by the live schema.
    let legacy_tools_register = json!({
        "tools": [{
            "name": "lookup",
            "description": "Look up a ticket",
            "readonly": true,
            "parameters": {"type": "object"}
        }]
    });
    assert!(
        mare_protocol::validate_schema_at(&contract_methods["tools.register"]["params"], "#", &legacy_tools_register)
            .is_err(),
        "tools.register schema must reject legacy tools/readonly payload"
    );
}

#[test]
fn protocol_enums_and_error_shapes_are_unified() {
    let proto = mare_protocol::schema();

    // No hyphenated enum values anywhere in mare-protocol schema.
    fn collect_hyphen_enums(v: &Value, path: &str, out: &mut Vec<String>) {
        match v {
            Value::Object(map) => {
                if let Some(vals) = map.get("enum").and_then(Value::as_array) {
                    for item in vals {
                        if let Some(s) = item.as_str()
                            && s.contains('-')
                        {
                            out.push(format!("{path}={s}"));
                        }
                    }
                }
                for (k, child) in map {
                    collect_hyphen_enums(child, &format!("{path}.{k}"), out);
                }
            }
            Value::Array(arr) => {
                for (i, child) in arr.iter().enumerate() {
                    collect_hyphen_enums(child, &format!("{path}[{i}]"), out);
                }
            }
            _ => {}
        }
    }
    let mut bad_enums = Vec::new();
    collect_hyphen_enums(&proto, "protocol", &mut bad_enums);
    assert!(bad_enums.is_empty(), "protocol enums must be snake_case without '-': {bad_enums:?}");

    let defs = proto.pointer("/notifications/event/$defs").expect("event $defs");
    for ev_name in ["TurnCompleted", "StepCompleted", "SubagentCompleted"] {
        let props = defs
            .get(ev_name)
            .and_then(|d| d.get("properties"))
            .unwrap_or_else(|| panic!("missing {ev_name} in protocol schema"));
        let err_schema = props.get("error").unwrap_or_else(|| panic!("{ev_name} missing error"));
        assert!(
            err_schema.to_string().contains("EventError"),
            "{ev_name}.error must be structured EventError, got {err_schema}"
        );
        assert!(props.get("error_kind").is_some(), "{ev_name} must declare error_kind: {props}");
        if ev_name == "StepCompleted" {
            assert!(props.get("kind").is_none(), "StepCompleted must use error_kind, not kind: {props}");
        }
    }

    let all = contracts();
    for (svc, meth, ptr) in [
        ("agent", "wait", "/properties/error"),
        ("subagent", "wait", "/properties/error"),
        ("subagent", "get", "/properties/error"),
        ("subagent", "start", "/properties/error"),
        ("subagent", "list", "/properties/data/items/properties/error"),
    ] {
        let res = &all[svc].methods[meth].result;
        let err = res.pointer(ptr).unwrap_or_else(|| panic!("{svc}.{meth} missing error at {ptr}: {res}"));
        assert_eq!(err["type"], "object", "{svc}.{meth} error must be structured object: {err}");
        assert!(
            err.pointer("/properties/code").is_some() && err.pointer("/properties/message").is_some(),
            "{svc}.{meth} error must have code and message: {err}"
        );
        let kind_ptr = ptr.replace("/error", "/error_kind");
        assert!(
            res.pointer(&kind_ptr).is_some(),
            "{svc}.{meth} must declare error_kind alongside structured error: {res}"
        );
    }
}

#[test]
fn session_surface_has_usable_search_items_unified_leaf_ids_and_no_legacy_duplicates() {
    let all = contracts();
    let sess = &all["session"];

    // Item 6: session.search returns item_id (usable with session.get_item) and does not require event_id.
    let hit = sess.methods["search"].result.pointer("/properties/data/items").expect("session.search hit schema");
    assert!(hit.pointer("/properties/item_id").is_some(), "session.search hit must expose item_id: {hit}");
    assert!(hit.pointer("/properties/event_id").is_none(), "session.search hit must drop unusable event_id: {hit}");

    // Item 9: alias and 5 parallel duplicate methods must be removed.
    for removed in ["id_of", "record", "records", "set_name", "rewind_conversation", "tool_state"] {
        assert!(
            !sess.methods.contains_key(removed),
            "removed alias/parallel method session.{removed} still exists in contract"
        );
    }

    // Item 7 & 8: session:info() uses session_id / session_epoch / leaf_entry_id; wire & disk unify on leaf_entry_id.
    let info_props = sess.methods["info"].result["properties"].as_object().expect("session.info properties");
    for expected in ["session_id", "session_epoch", "leaf_entry_id"] {
        assert!(info_props.contains_key(expected), "session.info missing {expected}: {info_props:?}");
    }
    for retired in ["id", "generation", "leaf"] {
        assert!(!info_props.contains_key(retired), "session.info still exposes retired {retired}: {info_props:?}");
    }

    // Zero leaf_id / source_leaf_id across all contracts.
    let mut bad_leaf = Vec::new();
    for (svc, contract) in all {
        for (meth, spec) in &contract.methods {
            for root in [&spec.params, &spec.result] {
                walk_schema_properties(root, &mut |prop, _| {
                    if matches!(prop, "leaf_id" | "source_leaf_id" | "leaf") {
                        bad_leaf.push(format!("{svc}.{meth}.{prop}"));
                    }
                });
            }
        }
        for ev in contract.events.values() {
            walk_schema_properties(&ev.data, &mut |prop, _| {
                if matches!(prop, "leaf_id" | "source_leaf_id" | "leaf") {
                    bad_leaf.push(format!("event:{}.{prop}", ev.name));
                }
            });
        }
    }
    assert!(bad_leaf.is_empty(), "contracts must unify on leaf_entry_id / source_leaf_entry_id: {bad_leaf:?}");
}

#[test]
fn handwritten_lua_types_satisfy_unified_vocabulary() {
    let types_dir = preset().join("types/mare");

    let approvals = std::fs::read_to_string(types_dir.join("approvals.lua")).unwrap();
    assert!(
        approvals.contains("---@class mare.ApprovalAuditCall\n---@field tool_call_id string"),
        "ApprovalAuditCall must use tool_call_id"
    );

    let net = std::fs::read_to_string(types_dir.join("net.lua")).unwrap();
    assert!(net.contains("---@field started_at_ms integer"), "PeerEntry must use started_at_ms");
    assert!(!net.contains("---@field started "), "PeerEntry must not use bare started");
    assert!(net.contains("plugins.session = {session_id"), "PeerEntry plugins.session doc must use session_id");

    let store = std::fs::read_to_string(types_dir.join("store.lua")).unwrap();
    assert!(!store.contains("log-only"), "types/mare/store.lua must use log_only, not log-only");
    assert!(!store.contains("migration backup"), "types/mare/store.lua must not mention migration backup");

    let models = std::fs::read_to_string(types_dir.join("services/models.lua")).unwrap();
    assert!(models.contains("---@class mare.JobInfo\n---@field job_id string"), "JobInfo must use job_id");
    assert!(models.contains("---@field started_at_ms integer"), "JobInfo must use started_at_ms");
    assert!(models.contains("---@field ended_at_ms integer|nil"), "JobInfo must use ended_at_ms");
    assert!(models.contains("---@field exit_code integer|nil"), "JobInfo must use exit_code");
    assert!(!models.contains("\"killed\""), "JobInfo status must use cancelled, not killed");
    assert!(models.contains("---@class mare.QueuedInput\n---@field input_id string"), "QueuedInput must use input_id");
    assert!(
        models.contains("---@class mare.ConversationInfo\n---@field conversation_id string"),
        "ConversationInfo must use conversation_id"
    );

    // Scan every handwritten .lua file under types/mare/ (excluding *.gen.lua) for retired field names.
    fn walk_lua(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "contracts") {
                    continue;
                }
                walk_lua(&p, out);
            } else if p.extension().is_some_and(|e| e == "lua")
                && !p.file_name().unwrap().to_string_lossy().ends_with(".gen.lua")
            {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk_lua(&types_dir, &mut files);
    let mut bad = Vec::new();
    for path in files {
        let rel = path.strip_prefix(&types_dir).unwrap().display().to_string();
        let text = std::fs::read_to_string(&path).unwrap();
        for (idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("---@field ") {
                let field = rest.split_whitespace().next().unwrap_or("").trim_end_matches('?');
                if matches!(field, "generation" | "started" | "ended" | "leaf_id" | "source_leaf_id") {
                    bad.push(format!("{rel}:{}:@field {field}", idx + 1));
                }
            }
        }
    }
    assert!(bad.is_empty(), "retired @field names in handwritten types/mare: {bad:?}");
}

#[test]
fn permissions_public_rpc_and_commands_run_session_id_are_locked() {
    use mare_kernel::contracts::{Access, Expose, Scope};

    let all = contracts();
    let perms = all.get("permissions").expect("permissions contract missing");
    for (name, expected_access, expected_admin) in
        [("get", Access::Peer, false), ("list", Access::Peer, false), ("set", Access::Primary, true)]
    {
        let spec = perms.methods.get(name).unwrap_or_else(|| panic!("permissions.{name} missing"));
        assert_eq!(spec.expose, Expose::Both, "permissions.{name} must be expose = both");
        assert_eq!(spec.scope, Scope::Session, "permissions.{name} must be scope = session");
        assert_eq!(spec.access, expected_access, "permissions.{name} access mismatch");
        assert_eq!(spec.admin, expected_admin, "permissions.{name} admin mismatch");
        assert!(!spec.lua_unchecked, "permissions.{name} must validate both Lua and RPC via the same contract");
        let wire = spec.wire_params();
        assert!(wire.pointer("/properties/session_id").is_some(), "permissions.{name} wire params need session_id");
        let req = wire["required"].as_array().expect("wire required");
        assert!(req.iter().any(|v| v == "session_id"), "permissions.{name} wire params must require session_id");
    }

    let list_res = &perms.methods["list"].result;
    assert_eq!(list_res.pointer("/properties/data/type"), Some(&json!("array")));
    let get_props = perms.methods["get"].result["properties"].as_object().expect("permissions.get properties");
    for field in ["preset", "label", "partial", "rules", "grants", "policy_revision"] {
        assert!(get_props.contains_key(field), "permissions.get missing field `{field}`");
    }
    assert_eq!(
        perms.methods["set"].result, perms.methods["get"].result,
        "permissions.set must return the same permission_state schema as permissions.get"
    );

    let cmds = all.get("commands").expect("commands contract missing");
    let run = cmds.methods.get("run").expect("commands.run missing");
    assert_eq!(run.expose, Expose::Both, "commands.run must be expose = both");
    assert_eq!(run.access, Access::Primary, "commands.run must be access = primary");
    assert!(run.admin, "commands.run must remain admin = true");
    assert_eq!(run.scope, Scope::Host, "commands.run stays scope = host with optional session_id");
    let run_wire = run.wire_params();
    assert_eq!(
        run_wire.pointer("/properties/session_id/type"),
        Some(&json!("string")),
        "commands.run must accept optional string session_id"
    );
    let run_req = run_wire["required"].as_array().expect("commands.run required");
    assert!(!run_req.iter().any(|v| v == "session_id"), "commands.run session_id must remain optional: {run_req:?}");
    assert!(run_req.iter().any(|v| v == "line"), "commands.run must still require line");
}
