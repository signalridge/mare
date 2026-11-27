//! Regression tests for the open, owner-partitioned `require` module loader.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use crate::common::*;

fn write_bumped(path: &Path, code: &str, secs: u64) {
    std::fs::write(path, code).unwrap();
    let t = std::time::SystemTime::now() + std::time::Duration::from_secs(secs);
    std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
}

fn repo_root() -> std::path::PathBuf {
    dunce::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

fn find_python() -> &'static str {
    for candidate in ["python3", "python"] {
        if Command::new(candidate).arg("--version").output().is_ok_and(|out| out.status.success()) {
            return candidate;
        }
    }
    panic!("python3 or python is required");
}

fn scan_types_meta(types_dir: &Path) -> BTreeSet<String> {
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
    let mut out = BTreeSet::new();
    walk(types_dir, &mut out);
    out
}

/// 1. Same-named modules (`lua/util.lua`) in different plugins must not collide in loaded cache.
#[tokio::test]
async fn same_named_modules_are_isolated_per_plugin() {
    local(async {
        let env = TestEnv::new().await;
        let alpha = env.config_dir().join("plugins/alpha");
        let beta = env.config_dir().join("plugins/beta");
        std::fs::create_dir_all(alpha.join("lua")).unwrap();
        std::fs::create_dir_all(beta.join("lua")).unwrap();
        std::fs::write(alpha.join("lua/util.lua"), r#"return { tag = "alpha" }"#).unwrap();
        std::fs::write(beta.join("lua/util.lua"), r#"return { tag = "beta" }"#).unwrap();
        std::fs::write(
            alpha.join("init.lua"),
            r#"return { name = "alpha", apply = function()
                mare._req_alpha = require("util")
            end }"#,
        )
        .unwrap();
        std::fs::write(
            beta.join("init.lua"),
            r#"return { name = "beta", apply = function()
                mare._req_beta = require("util")
            end }"#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        let (a_tag, b_tag, same_tbl): (String, String, bool) =
            eval(&h, r#"return mare._req_alpha.tag, mare._req_beta.tag, rawequal(mare._req_alpha, mare._req_beta)"#)
                .await;
        h.shutdown().await;

        assert_eq!(a_tag, "alpha");
        assert_eq!(b_tag, "beta");
        assert!(!same_tbl, "alpha and beta must get distinct util tables");
    })
    .await;
}

/// 2. Two plugins providing the same cross-plugin module root must error and name both providers.
#[tokio::test]
async fn ambiguous_cross_plugin_root_errors_and_names_providers() {
    local(async {
        let env = TestEnv::new().await;
        for prov in ["prov-a", "prov-b"] {
            let dir = env.config_dir().join(format!("plugins/{prov}"));
            std::fs::create_dir_all(dir.join("lua/shared_root")).unwrap();
            std::fs::write(dir.join("lua/shared_root/mod.lua"), format!(r#"return {{ from = "{prov}" }}"#)).unwrap();
            std::fs::write(dir.join("init.lua"), format!(r#"return {{ name = "{prov}", apply = function() end }}"#))
                .unwrap();
        }
        let consumer = env.config_dir().join("plugins/consumer");
        std::fs::create_dir_all(&consumer).unwrap();
        std::fs::write(
            consumer.join("init.lua"),
            r#"return { name = "consumer", apply = function()
                local ok, err = pcall(require, "shared_root.mod")
                mare._req_ambig = { ok = ok, err = tostring(err) }
            end }"#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        let (ok, err): (bool, String) = eval(&h, "return mare._req_ambig.ok, mare._req_ambig.err").await;
        h.shutdown().await;

        assert!(!ok, "ambiguous cross-plugin require must fail, got ok=true ({err})");
        assert!(
            err.contains("prov-a") && err.contains("prov-b"),
            "ambiguity error must name both providers (prov-a and prov-b): {err}"
        );
    })
    .await;
}

/// 3. Reloading a provider plugin must also reload plugins that required its modules.
// guarantee: dependents-stop-before-their-provider-and-restart-on-the-new-one
#[tokio::test]
async fn dependents_reload_with_their_provider() {
    local(async {
        let env = TestEnv::new().await;
        let alpha = env.config_dir().join("plugins/alpha");
        let beta = env.config_dir().join("plugins/beta");
        std::fs::create_dir_all(alpha.join("lua/alpha")).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let alpha_mod = alpha.join("lua/alpha/x.lua");
        std::fs::write(&alpha_mod, "return { ver = 1 }").unwrap();
        std::fs::write(alpha.join("init.lua"), r#"return { name = "alpha", apply = function() end }"#).unwrap();
        std::fs::write(
            beta.join("init.lua"),
            r#"return { name = "beta", apply = function()
                local x = require("alpha.x")
                mare._req_beta_ver = x.ver
                mare._req_beta_applies = (mare._req_beta_applies or 0) + 1
            end }"#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        let (v1, n1): (i64, i64) = eval(&h, "return mare._req_beta_ver, mare._req_beta_applies").await;
        assert_eq!((v1, n1), (1, 1));

        write_bumped(&alpha_mod, "return { ver = 2 }", 5);
        h.kernel.reload(Some("alpha")).await.unwrap();

        let (v2, n2): (i64, i64) = eval(&h, "return mare._req_beta_ver, mare._req_beta_applies").await;
        h.shutdown().await;

        assert_eq!(v2, 2, "dependent plugin beta must see alpha.x ver=2 after /reload alpha");
        assert!(n2 >= 2, "dependent plugin beta must have re-applied after /reload alpha (applies={n2})");
    })
    .await;
}

/// 4. Reloading alpha clears only alpha's module partition and leaves beta's partition untouched.
#[tokio::test]
async fn reload_clears_only_the_owners_partition() {
    local(async {
        let env = TestEnv::new().await;
        let alpha = env.config_dir().join("plugins/alpha");
        let beta = env.config_dir().join("plugins/beta");
        std::fs::create_dir_all(alpha.join("lua")).unwrap();
        std::fs::create_dir_all(beta.join("lua")).unwrap();
        std::fs::write(alpha.join("lua/util.lua"), r#"return { owner = "alpha" }"#).unwrap();
        std::fs::write(beta.join("lua/util.lua"), r#"return { owner = "beta" }"#).unwrap();
        std::fs::write(
            alpha.join("init.lua"),
            r#"return { name = "alpha", apply = function()
                mare._req_a_tbl = require("util")
            end }"#,
        )
        .unwrap();
        std::fs::write(
            beta.join("init.lua"),
            r#"return { name = "beta", apply = function()
                mare._req_b_tbl = require("util")
                mare._req_b_lookup = function() return require("util") end
            end }"#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        exec(&h, "mare._req_a_before = mare._req_a_tbl; mare._req_b_before = mare._req_b_tbl").await;
        h.kernel.reload(Some("alpha")).await.unwrap();

        let (a_reloaded, b_kept, b_owner): (bool, bool, String) = eval(
            &h,
            r#"local b_now = mare._req_b_lookup()
               return not rawequal(mare._req_a_tbl, mare._req_a_before),
                      rawequal(b_now, mare._req_b_before),
                      tostring(b_now.owner)"#,
        )
        .await;
        h.shutdown().await;

        assert!(a_reloaded, "reloading alpha must discard alpha's cached util table");
        assert!(b_kept, "reloading alpha must not clear beta's cached util table");
        assert_eq!(b_owner, "beta");
    })
    .await;
}

/// 4b. Partitioned loaded check via `Kernel::is_module_loaded`.
#[tokio::test]
async fn reload_clears_only_the_owners_partition_via_accessor() {
    local(async {
        let env = TestEnv::new().await;
        let alpha = env.config_dir().join("plugins/alpha");
        let beta = env.config_dir().join("plugins/beta");
        std::fs::create_dir_all(alpha.join("lua")).unwrap();
        std::fs::create_dir_all(beta.join("lua")).unwrap();
        std::fs::write(alpha.join("lua/lazy_mod.lua"), r#"return { owner = "alpha" }"#).unwrap();
        std::fs::write(beta.join("lua/lazy_mod.lua"), r#"return { owner = "beta" }"#).unwrap();
        std::fs::write(
            alpha.join("init.lua"),
            r#"return { name = "alpha", apply = function()
                mare._req_a_load = function() return require("lazy_mod") end
            end }"#,
        )
        .unwrap();
        std::fs::write(
            beta.join("init.lua"),
            r#"return { name = "beta", apply = function()
                mare._req_b_load = function() return require("lazy_mod") end
            end }"#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        assert!(!h.kernel.is_module_loaded("alpha", "lazy_mod"));
        assert!(!h.kernel.is_module_loaded("beta", "lazy_mod"));
        exec(&h, "mare._req_a_load(); mare._req_b_load()").await;
        assert!(h.kernel.is_module_loaded("alpha", "lazy_mod"));
        assert!(h.kernel.is_module_loaded("beta", "lazy_mod"));

        h.kernel.reload(Some("alpha")).await.unwrap();
        assert!(!h.kernel.is_module_loaded("alpha", "lazy_mod"), "alpha's partition must be cleared");
        assert!(h.kernel.is_module_loaded("beta", "lazy_mod"), "beta's partition must remain loaded");
        h.shutdown().await;
    })
    .await;
}

/// 5. A circular `require` within a plugin reports a clear `require cycle` error naming the chain.
#[tokio::test]
async fn in_plugin_require_cycle_reports_the_cycle() {
    local(async {
        let env = TestEnv::new().await;
        let cyc = env.config_dir().join("plugins/cyc");
        std::fs::create_dir_all(cyc.join("lua/cyc")).unwrap();
        std::fs::write(cyc.join("lua/cyc/a.lua"), r#"return require("cyc.b")"#).unwrap();
        std::fs::write(cyc.join("lua/cyc/b.lua"), r#"return require("cyc.a")"#).unwrap();
        std::fs::write(
            cyc.join("init.lua"),
            r#"return { name = "cyc", apply = function()
                local ok, err = pcall(require, "cyc.a")
                mare._req_cyc = { ok = ok, err = tostring(err) }
            end }"#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        let (ok, err): (bool, String) = eval(&h, "return mare._req_cyc.ok, mare._req_cyc.err").await;
        h.shutdown().await;

        assert!(!ok, "cyclic require must fail");
        assert!(
            err.contains("require cycle") && err.contains("cyc.a") && err.contains("cyc.b"),
            "cycle error must report 'require cycle' naming both cyc.a and cyc.b: {err}"
        );
    })
    .await;
}

/// 6. `mare_lib` is one core-owned instance shared by all plugins and survives a caller's reload.
#[tokio::test]
async fn mare_lib_is_one_core_owned_instance() {
    local(async {
        let env = TestEnv::new().await;
        for name in ["p1", "p2"] {
            let dir = env.config_dir().join(format!("plugins/{name}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("init.lua"),
                format!(
                    r#"return {{ name = "{name}", apply = function()
                        local ok, mod = pcall(require, "mare_lib")
                        mare._req_{name}_ok = ok
                        mare._req_{name}_mod = mod
                        mare._req_{name}_again = function() return require("mare_lib") end
                    end }}"#
                ),
            )
            .unwrap();
        }

        let h = env.host().user_plugins().no_session().build().await;
        let (ok1, ok2, same_initial): (bool, bool, bool) = eval(
            &h,
            r#"mare._req_saved = mare._req_p2_mod
               return mare._req_p1_ok, mare._req_p2_ok, rawequal(mare._req_p1_mod, mare._req_p2_mod)"#,
        )
        .await;
        assert!(ok1 && ok2, "both user plugins must be able to require mare_lib (ok1={ok1}, ok2={ok2})");
        assert!(same_initial, "p1 and p2 must receive the same core-owned mare_lib table");

        h.kernel.reload(Some("p1")).await.unwrap();
        let (p2_same_after, p1_same_after): (bool, bool) = eval(
            &h,
            r#"return rawequal(mare._req_p2_again(), mare._req_saved),
                      rawequal(mare._req_p1_mod, mare._req_saved)"#,
        )
        .await;
        h.kernel.reload(Some("commands")).await.unwrap();
        let survives_builtin_reload: bool = eval(&h, "return rawequal(mare._req_p2_again(), mare._req_saved)").await;
        let in_core_partition = h.kernel.is_module_loaded("core", "mare_lib");
        h.shutdown().await;

        assert!(p2_same_after, "reloading p1 must not evict core-owned mare_lib");
        assert!(p1_same_after, "p1 after reload must still receive the same core-owned mare_lib");
        assert!(survives_builtin_reload, "reloading a built-in must not evict core-owned mare_lib");
        assert!(in_core_partition, "mare_lib must live in the core partition (package.loaded)");
    })
    .await;
}

/// Test 7: Single source of truth: generated public module table matches `preset/types/**/*.lua`
/// `---@meta` declarations and `scripts/lua_api_rules.py`.
#[test]
fn public_module_table_matches_types_meta() {
    let root = repo_root();
    let scanned = scan_types_meta(&root.join("crates/mare-plugin/preset/types"));
    assert!(scanned.contains("cordis") && scanned.contains("mare_lib"), "expected core ---@meta modules: {scanned:?}");

    let _ = mare_plugin::loader::builtin_sources();
    let generated_plugin: BTreeSet<String> =
        mare_plugin::loader::PUBLIC_MODULES.iter().map(|s| (*s).to_string()).collect();
    let generated_kernel: BTreeSet<String> =
        mare_kernel::Kernel::public_modules().iter().map(|s| (*s).to_string()).collect();
    assert_eq!(generated_plugin, scanned, "mare_plugin::loader::PUBLIC_MODULES must match types/ ---@meta scan");
    assert_eq!(generated_kernel, scanned, "Kernel::public_modules() must match types/ ---@meta scan");

    // Verify scripts/lua_api_rules.py exposes public_modules() matching the same types/ scan.
    let out = Command::new(find_python())
        .args([
            "-B",
            "-c",
            r#"import json, sys; from pathlib import Path
sys.path.insert(0, "scripts")
import lua_api_rules
print(json.dumps(sorted(lua_api_rules.public_modules(Path("crates/mare-plugin/preset/types")))))"#,
        ])
        .current_dir(&root)
        .output()
        .expect("run python public_modules check");
    assert!(
        out.status.success(),
        "lua_api_rules.public_modules() check failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let py_mods: BTreeSet<String> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(py_mods, scanned, "lua_api_rules.public_modules() must equal types/ ---@meta scan");
}

/// Test 8: Cross-plugin-required modules holding mutable module-level state or registering listeners
/// at require time must trigger a lint warning; pure stateless shared modules must not.
#[test]
fn cross_plugin_required_stateful_module_triggers_lint_warning() {
    let root = repo_root();
    let script = root.join("scripts/lua_api_require_state.py");
    assert!(script.is_file(), "missing cross-plugin require state lint script: {}", script.display());

    let tmp = tempfile::tempdir().unwrap();
    let plugins = tmp.path().join("plugins");
    let alpha_lua = plugins.join("alpha/lua/alpha");
    let beta = plugins.join("beta");
    std::fs::create_dir_all(&alpha_lua).unwrap();
    std::fs::create_dir_all(&beta).unwrap();

    // Stateful shared module: mutable module-level table + require-time listener registration.
    std::fs::write(
        alpha_lua.join("stateful.lua"),
        r#"local cache = {}
local count = 0
local M = {}
function M.put(k, v)
    count = count + 1
    cache[k] = v
end
mare.on("turn/started", function() end)
return M
"#,
    )
    .unwrap();
    // Pure stateless shared module: constants + pure functions only.
    std::fs::write(
        alpha_lua.join("pure.lua"),
        r#"local PREFIX = "ok:"
local M = {}
function M.format(x)
    return PREFIX .. tostring(x)
end
return M
"#,
    )
    .unwrap();
    std::fs::write(plugins.join("alpha/init.lua"), r#"return { name = "alpha", apply = function() end }"#).unwrap();
    std::fs::write(
        beta.join("init.lua"),
        r#"return { name = "beta", apply = function()
            local s = require("alpha.stateful")
            local p = require("alpha.pure")
            return s, p
        end }"#,
    )
    .unwrap();

    let out = Command::new(find_python())
        .args(["-B", script.to_str().unwrap(), plugins.to_str().unwrap()])
        .current_dir(&root)
        .output()
        .expect("run lua_api_require_state.py");
    let combined = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "state rule lint emits warnings (exit 0), got {:?}:\n{combined}", out.status.code());
    assert!(
        combined.contains("alpha.stateful") && combined.contains("warning"),
        "expected warning for stateful cross-plugin module alpha.stateful:\n{combined}"
    );
    assert!(
        !combined.contains("alpha.pure"),
        "pure stateless shared module alpha.pure must not trigger a warning:\n{combined}"
    );
}

/// A user plugin can require `cordis`, `mare_kernel.*`, and another
/// plugin's stateless module under the open require model.
#[tokio::test]
async fn a_user_plugin_requires_kernel_and_other_plugin_modules() {
    local(async {
        let env = TestEnv::new().await;
        let lib = env.config_dir().join("plugins/helperlib");
        let usr = env.config_dir().join("plugins/usr");
        std::fs::create_dir_all(lib.join("lua/helperlib")).unwrap();
        std::fs::create_dir_all(&usr).unwrap();
        std::fs::write(lib.join("lua/helperlib/math.lua"), "return { double = function(x) return x * 2 end }").unwrap();
        std::fs::write(lib.join("init.lua"), r#"return { name = "helperlib", apply = function() end }"#).unwrap();
        std::fs::write(
            usr.join("init.lua"),
            r#"return { name = "usr", apply = function()
                local cordis = require("cordis")
                local calls = require("mare_kernel.calls")
                local mathlib = require("helperlib.math")
                mare._req_open = {
                    cordis = cordis.FiberState.ACTIVE == 2,
                    kernel = type(calls.inactive) == "function",
                    val = mathlib.double(21),
                }
            end }"#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        let (c_ok, k_ok, val): (bool, bool, i64) =
            eval(&h, "return mare._req_open.cordis, mare._req_open.kernel, mare._req_open.val").await;
        h.shutdown().await;

        assert!(c_ok, "user plugin must be able to require cordis");
        assert!(k_ok, "user plugin must be able to require mare_kernel.*");
        assert_eq!(val, 42, "user plugin must be able to require another plugin's module");
    })
    .await;
}

/// `package.preload` overrides module loading for every owner.
#[tokio::test]
async fn package_preload_overrides_every_owner() {
    local(async {
        let env = TestEnv::new().await;
        let alpha = env.config_dir().join("plugins/alpha");
        std::fs::create_dir_all(alpha.join("lua")).unwrap();
        std::fs::write(alpha.join("lua/util.lua"), r#"return { from = "disk" }"#).unwrap();
        std::fs::write(
            alpha.join("init.lua"),
            r#"return { name = "alpha", apply = function()
                package.preload["util"] = function() return { from = "preload" } end
                mare._req_pre = require("util").from
            end }"#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        let from: String = eval(&h, "return mare._req_pre").await;
        h.shutdown().await;

        assert_eq!(from, "preload", "package.preload must take precedence over plugin lua/");
    })
    .await;
}

/// A plugin defining a core-reserved root under `lua/` records a
/// doctor problem naming the plugin and root, and core modules still resolve to `core`.
#[tokio::test]
async fn reserved_core_root_in_plugin_records_doctor_problem() {
    local(async {
        let env = TestEnv::new().await;
        let rogue = env.config_dir().join("plugins/rogue");
        std::fs::create_dir_all(rogue.join("lua/mare_lib")).unwrap();
        std::fs::write(rogue.join("lua/mare_lib/init.lua"), r#"return { hijacked = true }"#).unwrap();
        std::fs::write(
            rogue.join("init.lua"),
            r#"return { name = "rogue", apply = function()
                mare._req_rogue_lib = require("mare_lib")
            end }"#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build_unchecked_exposed().await;
        let hijacked: bool = eval(&h, "return mare._req_rogue_lib.hijacked == true").await;
        let problems = h.kernel.problems();
        h.shutdown().await;

        assert!(!hijacked, "core root `mare_lib` must not be hijacked by a plugin");
        assert!(
            problems.iter().any(|p| p.contains("rogue") && p.contains("mare_lib")),
            "expected doctor problem naming `rogue` and `mare_lib`, got: {problems:?}"
        );
    })
    .await;
}

/// An `init.lua` containing closures (carried/fingerprinted via
/// `carry.rs` and `config.rs`) still loads and reloads cleanly alongside per-plugin `require`.
#[tokio::test]
async fn init_lua_closure_with_require_still_serializes_and_reloads() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"local prefix = "req-ok:"
mare.plugin("alpha", {
    cb = function(x) return prefix .. tostring(x) end,
})
"#,
        )
        .unwrap();
        let alpha = env.config_dir().join("plugins/alpha");
        std::fs::create_dir_all(alpha.join("lua/alpha")).unwrap();
        std::fs::write(alpha.join("lua/alpha/tag.lua"), r#"return { val = "v1" }"#).unwrap();
        std::fs::write(
            alpha.join("init.lua"),
            r#"return {
                name = "alpha",
                config_schema = {
                    cb = { type = "function", scope = "lua", desc = "a callback" },
                },
                apply = function(ctx, opts)
                    local tag = require("alpha.tag")
                    mare._req_closure_tag = tag.val
                    mare._req_closure_cb = opts.cb
                    mare._req_closure_applies = (mare._req_closure_applies or 0) + 1
                end,
            }"#,
        )
        .unwrap();

        let h = env.host().user_config(false).no_session().build().await;
        let (fn_res, tag, applies1): (String, String, i64) =
            eval(&h, r#"return mare._req_closure_cb("a"), mare._req_closure_tag, mare._req_closure_applies"#).await;
        assert_eq!(fn_res, "req-ok:a");
        assert_eq!(tag, "v1");
        assert_eq!(applies1, 1);

        h.reload_config().await.unwrap();
        let (fn_res2, applies2): (String, i64) =
            eval(&h, r#"return mare._req_closure_cb("b"), mare._req_closure_applies"#).await;
        h.shutdown().await;

        assert_eq!(fn_res2, "req-ok:b");
        assert_eq!(applies2, 1, "unchanged init.lua closure must not re-apply alpha on reload_config");
    })
    .await;
}

/// Modules already loaded during `apply` hit the in-memory
/// partition cache without touching disk or FS policy, so calling `require` again inside a
/// `restrict_read` tool whose allowed roots exclude the plugin root still succeeds.
#[tokio::test]
async fn preloaded_module_succeeds_under_restrict_read_excluding_plugin_root() {
    local(async {
        let env = TestEnv::new().await;
        let helper = env.config_dir().join("plugins/helper");
        let alpha = env.config_dir().join("plugins/alpha");
        std::fs::create_dir_all(helper.join("lua/helper")).unwrap();
        std::fs::create_dir_all(alpha.join("lua/alpha")).unwrap();
        std::fs::write(helper.join("lua/helper/fmt.lua"), r#"return { wrap = function(s) return "[" .. s .. "]" end }"#)
            .unwrap();
        std::fs::write(helper.join("init.lua"), r#"return { name = "helper", apply = function() end }"#).unwrap();
        std::fs::write(alpha.join("lua/alpha/view.lua"), r#"return { render = function() return "frame-ok" end }"#)
            .unwrap();
        std::fs::write(alpha.join("lua/alpha/lazy.lua"), r#"return { lazy = true }"#).unwrap();
        std::fs::write(
            alpha.join("init.lua"),
            r#"return {
                name = "alpha",
                inject = { "tools" },
                apply = function(ctx)
                    -- Preload both own module and cross-plugin module during apply.
                    assert(require("alpha.view").render() == "frame-ok")
                    assert(require("helper.fmt").wrap("x") == "[x]")
                    ctx.tools:register("req_restricted_probe", {
                        description = "probe cached require under restrict_read",
                        parameters = { type = "object", properties = {} },
                        execute = function()
                            local own = require("alpha.view").render()
                            local cross = require("helper.fmt").wrap(own)
                            local lazy_ok = pcall(require, "alpha.lazy")
                            return cross .. "|lazy=" .. tostring(lazy_ok)
                        end,
                    })
                end,
            }"#,
        )
        .unwrap();

        let script =
            serde_json::json!([{ "tool_calls": [{ "name": "req_restricted_probe", "arguments": {} }] }, { "text": "done" }]);
        let h = env.host().faux(script).user_plugins().no_session().build().await;
        exec(
            &h,
            r#"mare.proc.sandbox.set_current({ level = "workspace-write", restrict_read = true,
                reads_pinned = true, readable = { "." } })"#,
        )
        .await;
        let dump = h.prompt("go", serde_json::json!({})).await.unwrap().to_string();
        h.shutdown().await;

        assert!(
            dump.contains("[frame-ok]|lazy=false"),
            "preloaded own+cross modules must succeed under restrict_read while unloaded module stays blocked: {dump}"
        );
    })
    .await;
}

/// Row-driven reload (`reload_config`) restarts a dependent plugin
/// once when its provider's modules change, and does NOT restart it a second time on the next
/// no-op reload.
#[tokio::test]
async fn row_driven_reload_reloads_dependents_once_and_does_not_restart_again() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let alpha = env.config_dir().join("plugins/alpha");
        let beta = env.config_dir().join("plugins/beta");
        std::fs::create_dir_all(alpha.join("lua/alpha")).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let alpha_mod = alpha.join("lua/alpha/x.lua");
        std::fs::write(&alpha_mod, "return { ver = 1 }").unwrap();
        std::fs::write(alpha.join("init.lua"), r#"return { name = "alpha", apply = function() end }"#).unwrap();
        std::fs::write(
            beta.join("init.lua"),
            r#"return { name = "beta", apply = function()
                local x = require("alpha.x")
                mare._req_row_ver = x.ver
                mare._req_row_applies = (mare._req_row_applies or 0) + 1
            end }"#,
        )
        .unwrap();

        let h = env.host().user_config(false).no_session().build().await;
        let (v1, n1): (i64, i64) = eval(&h, "return mare._req_row_ver, mare._req_row_applies").await;
        assert_eq!((v1, n1), (1, 1));

        // No-op row-driven reload: beta must not restart.
        h.kernel.apply_rows_report().await.unwrap();
        let n_noop: i64 = eval(&h, "return mare._req_row_applies").await;
        assert_eq!(n_noop, 1, "no-op apply_rows_report must not restart beta");

        // Edit provider module and trigger row-driven reload: beta reloads once to ver = 2.
        write_bumped(&alpha_mod, "return { ver = 2 }", 5);
        h.kernel.apply_rows_report().await.unwrap();
        let (v2, n2): (i64, i64) = eval(&h, "return mare._req_row_ver, mare._req_row_applies").await;
        assert_eq!((v2, n2), (2, 2), "row-driven reload must restart beta once to pick up alpha.x ver=2");

        // Subsequent no-op row-driven reload: ROW_CACHE must reuse last fingerprint, so beta does NOT restart again!
        h.kernel.apply_rows_report().await.unwrap();
        let (v3, n3): (i64, i64) = eval(&h, "return mare._req_row_ver, mare._req_row_applies").await;
        h.shutdown().await;

        assert_eq!((v3, n3), (2, 2), "second no-op apply_rows_report must not restart beta again");
    })
    .await;
}

/// Every plugin that requires `mare_lib` (including the 2nd+ caller
/// that hits `package.loaded["mare_lib"]` at step 1) records a dependency edge to `core`.
#[tokio::test]
async fn second_caller_of_core_mare_lib_records_core_dependency() {
    local(async {
        let env = TestEnv::new().await;
        for name in ["c1", "c2"] {
            let dir = env.config_dir().join(format!("plugins/{name}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("init.lua"),
                format!(
                    r#"return {{ name = "{name}", apply = function()
                        assert(type(require("mare_lib")) == "table")
                    end }}"#
                ),
            )
            .unwrap();
        }

        let h = env.host().user_plugins().no_session().build().await;
        let deps = h.kernel.module_dependents("core");
        h.shutdown().await;

        assert!(deps.iter().any(|d| d == "c1"), "first caller c1 must be a dependent of core: {deps:?}");
        assert!(
            deps.iter().any(|d| d == "c2"),
            "second caller c2 (step-1 hit) must also be a dependent of core: {deps:?}"
        );
    })
    .await;
}

/// Named reload of a currently non-running provider plugin must still restart active
/// dependents that previously `require`d modules from it.
#[tokio::test]
async fn named_reload_of_non_running_provider_restarts_module_dependents() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let alpha = env.config_dir().join("plugins/alpha");
        let beta = env.config_dir().join("plugins/beta");
        std::fs::create_dir_all(alpha.join("lua/alpha")).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let alpha_mod = alpha.join("lua/alpha/x.lua");
        std::fs::write(&alpha_mod, "return { ver = 1 }").unwrap();
        std::fs::write(alpha.join("init.lua"), r#"return { name = "alpha", apply = function() end }"#).unwrap();
        std::fs::write(
            beta.join("init.lua"),
            r#"return { name = "beta", apply = function()
                local x = require("alpha.x")
                mare._nr_ver = x.ver
                mare._nr_applies = (mare._nr_applies or 0) + 1
            end }"#,
        )
        .unwrap();

        let h = env.host().user_config(false).no_session().build().await;
        let (v1, n1): (i64, i64) = eval(&h, "return mare._nr_ver, mare._nr_applies").await;
        assert_eq!((v1, n1), (1, 1));
        assert!(h.kernel.plugin_active("alpha"));
        assert!(h.kernel.plugin_active("beta"));

        // Disable alpha via config so alpha is not running while beta stays active.
        h.kernel.set_config(serde_json::json!({ "plugins": { "alpha": { "enabled": false } } }));
        h.kernel.apply_rows_report().await.unwrap();
        assert!(!h.kernel.plugin_active("alpha"), "alpha must not be running after disabling");
        assert!(h.kernel.plugin_active("beta"), "beta must remain active");
        assert!(
            h.kernel.module_dependents("alpha").iter().any(|d| d == "beta"),
            "beta must still be tracked as a module dependent of alpha"
        );

        // Record kernel/before-reload announcements during the named reload.
        exec(
            &h,
            r#"mare._nr_before = {}
               mare.on("kernel/before-reload", function(ev)
                   for _, p in ipairs(ev.plugins or {}) do
                       mare._nr_before[p] = true
                   end
               end)"#,
        )
        .await;

        // Update alpha's module on disk, re-enable alpha in config, and perform a named reload
        // while alpha is still not running in the tree.
        write_bumped(&alpha_mod, "return { ver = 2 }", 5);
        h.kernel.set_config(serde_json::json!({}));
        let report = h.kernel.reload_report(Some("alpha")).await.unwrap();
        let (v2, n2, saw_beta): (i64, i64, bool) =
            eval(&h, "return mare._nr_ver, mare._nr_applies, mare._nr_before.beta == true").await;
        let alpha_active = h.kernel.plugin_active("alpha");
        h.shutdown().await;

        assert!(alpha_active, "alpha must be active after named reload");
        assert!(report.failures.is_empty(), "named reload must succeed: {:?}", report.failures);
        assert!(
            report.applied.iter().any(|p| p == "beta"),
            "named reload of non-running alpha must report beta in applied: {:?}",
            report.applied
        );
        assert!(saw_beta, "kernel/before-reload must include module dependent beta");
        assert_eq!((v2, n2), (2, 2), "beta must restart and pick up alpha.x ver=2");
    })
    .await;
}

/// Cross-owner `require` executed lazily after `apply()` must incrementally record the
/// provider's module fingerprint so subsequent row-driven reloads (`/reload config`) restart
/// the consumer when the provider's module changes.
#[tokio::test]
async fn lazy_cross_owner_require_after_apply_tracks_dep_fingerprint() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let alpha = env.config_dir().join("plugins/alpha");
        let beta = env.config_dir().join("plugins/beta");
        std::fs::create_dir_all(alpha.join("lua/alpha")).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let alpha_mod = alpha.join("lua/alpha/x.lua");
        std::fs::write(&alpha_mod, "return { ver = 1 }").unwrap();
        std::fs::write(alpha.join("init.lua"), r#"return { name = "alpha", apply = function() end }"#).unwrap();
        std::fs::write(
            beta.join("init.lua"),
            r#"return { name = "beta", apply = function()
                local cached = nil
                mare._lazy_ver = function()
                    if cached == nil then
                        cached = require("alpha.x").ver
                    end
                    return cached
                end
                mare._lazy_applies = (mare._lazy_applies or 0) + 1
            end }"#,
        )
        .unwrap();

        let h = env.host().user_config(false).no_session().build().await;
        let n1: i64 = eval(&h, "return mare._lazy_applies").await;
        assert_eq!(n1, 1);
        assert!(!h.kernel.is_module_loaded("alpha", "alpha.x"), "alpha.x must stay cold during startup");

        // Trigger the first cross-owner require lazily AFTER apply() has finished.
        let v1: i64 = eval(&h, "return mare._lazy_ver()").await;
        assert_eq!(v1, 1);
        assert!(h.kernel.is_module_loaded("alpha", "alpha.x"));

        // No-op row-driven reload must not restart beta.
        h.kernel.apply_rows_report().await.unwrap();
        let n_noop: i64 = eval(&h, "return mare._lazy_applies").await;
        assert_eq!(n_noop, 1, "no-op apply_rows_report must not restart beta");

        // Edit alpha.x on disk and run row-driven reload: beta must restart and pick up ver=2.
        write_bumped(&alpha_mod, "return { ver = 2 }", 5);
        h.kernel.apply_rows_report().await.unwrap();
        let (v2, n2): (i64, i64) = eval(&h, "return mare._lazy_ver(), mare._lazy_applies").await;
        assert_eq!((v2, n2), (2, 2), "row-driven reload must restart beta after lazy cross-owner require");

        // Second no-op row-driven reload must not restart beta again.
        h.kernel.apply_rows_report().await.unwrap();
        let (v3, n3): (i64, i64) = eval(&h, "return mare._lazy_ver(), mare._lazy_applies").await;
        h.shutdown().await;

        assert_eq!((v3, n3), (2, 2), "second no-op apply_rows_report must not restart beta again");
    })
    .await;
}

/// If a provider already loaded its module into memory at startup, the file is edited on disk,
/// and a consumer then lazily requires it (hitting the in-memory fast path and receiving v1),
/// the consumer's recorded dep fingerprint must reflect the loaded v1 snapshot so the next
/// row-driven reload still restarts the consumer to pick up v2.
#[tokio::test]
async fn lazy_cross_owner_require_fast_path_hit_after_disk_edit_still_restarts_on_reload() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let alpha = env.config_dir().join("plugins/alpha");
        let beta = env.config_dir().join("plugins/beta");
        std::fs::create_dir_all(alpha.join("lua/alpha")).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let alpha_mod = alpha.join("lua/alpha/x.lua");
        std::fs::write(&alpha_mod, "return { ver = 1 }").unwrap();
        std::fs::write(
            alpha.join("init.lua"),
            r#"return { name = "alpha", apply = function()
                mare._fp_alpha = require("alpha.x").ver
            end }"#,
        )
        .unwrap();
        std::fs::write(
            beta.join("init.lua"),
            r#"return { name = "beta", apply = function()
                local cached = nil
                mare._fp_beta = function()
                    if cached == nil then
                        cached = require("alpha.x").ver
                    end
                    return cached
                end
                mare._fp_beta_applies = (mare._fp_beta_applies or 0) + 1
            end }"#,
        )
        .unwrap();

        let h = env.host().user_config(false).no_session().build().await;
        let (av1, bn1): (i64, i64) = eval(&h, "return mare._fp_alpha, mare._fp_beta_applies").await;
        assert_eq!((av1, bn1), (1, 1));

        // Edit alpha.x on disk BEFORE beta lazily requires it.
        write_bumped(&alpha_mod, "return { ver = 2 }", 5);

        // Beta lazily requires alpha.x -> hits alpha's in-memory partition (receives stale ver=1).
        let bv1: i64 = eval(&h, "return mare._fp_beta()").await;
        assert_eq!(bv1, 1, "fast-path hit before reload must return alpha's loaded v1 module");

        // Row-driven reload must detect that beta loaded v1 while disk is now v2, and restart beta.
        h.kernel.apply_rows_report().await.unwrap();
        let (av2, bv2, bn2): (i64, i64, i64) =
            eval(&h, "return mare._fp_alpha, mare._fp_beta(), mare._fp_beta_applies").await;
        h.shutdown().await;

        assert_eq!(av2, 2);
        assert_eq!((bv2, bn2), (2, 2), "beta must restart after fast-path lazy require saw pre-edit v1");
    })
    .await;
}
