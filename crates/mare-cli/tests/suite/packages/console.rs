//! The Lua console surface: mare.g, local-only commands and mare init.

use super::*;

#[tokio::test]
async fn mare_g_survives_reload_and_hands_out_copies() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        exec(&h, r#"mare.g.counter = { n = 1, tags = { "a" } }"#).await;
        // a read is a copy: changing it doesn't change the stored value
        let n: i64 = eval(&h, r#"local c = mare.g.counter; c.n = 99; return mare.g.counter.n"#).await;
        assert_eq!(n, 1);
        // only plain data
        let bad: String = eval(&h, r#"local ok, e = pcall(function() mare.g.f = print end); return tostring(e)"#).await;
        assert!(bad.contains("only stores"), "{bad}");
        h.kernel.reload(None).await.unwrap();
        let after: i64 = eval(&h, "return mare.g.counter.n").await;
        assert_eq!(after, 1, "kept across /reload");
        h.shutdown().await;
    })
    .await
}

#[test]
fn local_only_commands_are_refused_over_rpc() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("lo.lua"),
        r#"return { name = "lo", inject = { "commands" }, apply = function(ctx)
  ctx.commands:register("safe-target", { run = function() return "safe" end })
  ctx.commands:register("secret", { local_only = true, run = function() mare.g.ran = true end })
  ctx.commands:register("lua", { local_only = true, run = function() mare.g.ran_lua = true end })
  ctx.commands:register("secret-alias", { alias_of = "secret" })
  -- 3-hop alias with intermediate local_only node must still be rejected over RPC.
  ctx.commands:register("secret-hop2", { local_only = true, alias_of = "safe-target" })
  ctx.commands:register("secret-hop1", { alias_of = "secret-hop2" })
end }"#,
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(std::process::Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    for line in ["/secret", "/lua mare.g.x = 1", "/secret-alias", "/secret-hop1", "/lua\t1"] {
        let r = c.call_raw("commands.run", json!({ "line": line }), |_| json!(null));
        assert_eq!(r["error"]["data"]["code"], "commands.local_only", "{line}: {r}");
        assert!(
            r["error"]["message"].as_str().unwrap_or("").contains("only available in the terminal UI"),
            "{line}: {r}"
        );
    }
    let leading_ws = c.call_raw("commands.run", json!({ "line": " /lua" }), |_| json!(null));
    assert_eq!(leading_ws["error"]["data"]["code"], "invalid_arguments", "{leading_ws}");
    let upper = c.call_raw("commands.run", json!({ "line": "/LUA" }), |_| json!(null));
    assert_eq!(upper["error"]["data"]["code"], "commands.unknown_command", "{upper}");
    let check =
        c.call_raw("commands.register", json!({ "name": "probe-check", "callbacks": ["run"] }), |_| json!(null));
    assert_eq!(check["error"]["data"]["code"], "unsupported", "capabilities.callbacks required: {check}");
    c.shutdown_and_wait();
}

#[tokio::test]
async fn mare_init_keeps_a_luarc_the_user_changed() {
    local(async {
        let env = TestEnv::new().await;
        let luarc = env.config_dir().join(".luarc.json");
        let out = mare_plugin::runtime::init().unwrap().join("\n");
        assert!(out.contains("wrote") && luarc.exists(), "{out}");
        assert!(mare_plugin::runtime::init().unwrap().join("\n").contains("wrote"), "still mare's own: refreshed");
        std::fs::write(&luarc, "{ \"mine\": true }\n").unwrap();
        let out = mare_plugin::runtime::init().unwrap().join("\n");
        assert!(out.contains("kept") && out.contains(".luarc.json"), "{out}");
        assert_eq!(std::fs::read_to_string(&luarc).unwrap(), "{ \"mine\": true }\n");
    })
    .await
}

/// `mare runtime` extracts builtins + types and prints the runtime dir; `ensure()` prunes
/// stale `*.lua` files under `types/` when the version advances; `init()` writes `DEFAULT_INIT`
/// once and preserves user edits thereafter.
#[tokio::test]
async fn mare_runtime_cli_prunes_stale_types_and_init_writes_default_init_once() {
    local(async {
        let env = TestEnv::new().await;
        let out = mare_cmd().arg("runtime").env("MARE_HOME", env.home.path()).run_output();
        assert!(out.status.success(), "mare runtime failed: {}", String::from_utf8_lossy(&out.stderr));
        let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let rt_dir = std::path::PathBuf::from(&printed);
        assert!(rt_dir.join("VERSION").is_file(), "missing VERSION in {printed}");
        assert!(rt_dir.join("types/mare.lua").is_file(), "missing types/mare.lua in {printed}");

        // Plant a stale dropped type file and roll VERSION back so `ensure()` refreshes and prunes it.
        let stale = rt_dir.join("types/stale_dropped_ns.lua");
        std::fs::write(&stale, "-- stale\n").unwrap();
        std::fs::write(rt_dir.join("VERSION"), "0.0.0-stale").unwrap();
        mare_plugin::runtime::ensure().unwrap();
        assert!(!stale.exists(), "prune_types must remove stale *.lua files under runtime/types/");
        assert!(rt_dir.join("types/mare.lua").is_file(), "current shipped types must remain");

        // `init()` creates default `init.lua` once and never overwrites user edits.
        let init_path = env.config_dir().join("init.lua");
        assert!(!init_path.exists());
        let first = mare_plugin::runtime::init().unwrap().join("\n");
        assert!(first.contains("init.lua") && init_path.is_file(), "{first}");
        let initial = std::fs::read_to_string(&init_path).unwrap();
        assert!(initial.contains("mare.setup"), "DEFAULT_INIT must contain mare.setup: {initial}");
        std::fs::write(&init_path, "-- user custom init\n").unwrap();
        let second = mare_plugin::runtime::init().unwrap().join("\n");
        assert!(second.contains("kept"), "{second}");
        assert_eq!(std::fs::read_to_string(&init_path).unwrap(), "-- user custom init\n");
    })
    .await
}

/// Concurrent `mare runtime` processes refreshing a stale read-only runtime directory must
/// serialize instead of interleaving chmod/write/chmod on the same files.
#[tokio::test]
async fn mare_runtime_concurrent_processes_refresh_stale_dir_without_race() {
    local(async {
        let env = TestEnv::new().await;
        // First populate the runtime directory with read-only files.
        let seed = mare_cmd().arg("runtime").env("MARE_HOME", env.home.path()).run_output();
        assert!(seed.status.success(), "seed mare runtime failed: {}", String::from_utf8_lossy(&seed.stderr));
        let rt_dir = std::path::PathBuf::from(String::from_utf8_lossy(&seed.stdout).trim().to_string());
        let stamp = rt_dir.join("VERSION");

        for round in 0..3 {
            std::fs::write(&stamp, format!("0.0.0-stale-{round}")).unwrap();
            let children: Vec<_> = (0..4)
                .map(|_| {
                    mare_cmd()
                        .arg("runtime")
                        .env("MARE_HOME", env.home.path())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .spawn()
                        .expect("spawn mare runtime")
                })
                .collect();
            for (idx, child) in children.into_iter().enumerate() {
                let out = child.wait_with_output().expect("wait mare runtime");
                assert!(
                    out.status.success(),
                    "round {round} child {idx} failed ({}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                );
                assert_eq!(std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()), rt_dir);
            }
            assert_eq!(std::fs::read_to_string(&stamp).unwrap().trim(), env!("CARGO_PKG_VERSION"));
            assert!(rt_dir.join("types/mare.lua").is_file());
        }
    })
    .await
}

/// Plugin runner emits `plugins/completed` with `action` and `status`.
#[tokio::test]
async fn plugins_runner_emits_plugins_completed_event() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["plugins"]).no_session().build().await;
        expose(&h);
        let (action, status): (String, String) = eval(
            &h,
            r#"
            local seen = nil
            C.cordis:on("plugins/completed", function(ev) seen = ev end)
            local orig_check = mare.plugins.check
            mare.plugins.check = function() return { { name = "demo", status = "ok" } } end
            local extras_list = require("plugins.extras_list")
            local runner = extras_list.runner(C.cordis)
            local ok, err = pcall(runner, "check", nil, true)
            mare.plugins.check = orig_check
            assert(ok, tostring(err))
            return tostring(seen and seen.action or ""), tostring(seen and seen.status or "")
            "#,
        )
        .await;
        assert_eq!((action.as_str(), status.as_str()), ("check", "completed"));
        h.shutdown().await;
    })
    .await
}

/// `mare.o.news = false` suppresses startup news checking on `app/ready`.
#[tokio::test]
async fn mare_o_news_false_suppresses_startup_news_check() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .builtins(&["plugins"])
            .config(json!({ "news": false, "plugins": { "onboarding": { "plugins": false } } }))
            .no_session();
        b.opts.mode = mare_shared::ui::Mode::Tui;
        let h = b.build().await;
        expose(&h);
        exec(
            &h,
            r#"
            _G.__NEWS_CALLED = false
            local svc = C.cordis:get("plugins")
            local orig = svc.news
            svc.news = function(...) _G.__NEWS_CALLED = true return orig(...) end
            "#,
        )
        .await;
        h.kernel.emit_json("app/ready", json!({})).await;
        let called: bool = eval(&h, "mare.task.sleep(30); return _G.__NEWS_CALLED").await;
        assert!(!called, "mare.o.news = false must not run startup news check");
        h.shutdown().await;
    })
    .await
}

/// `mare.g` rejects cyclic tables and non-primitive values, and `pairs(mare.g)` iterates
/// over an isolated snapshot that is unaffected by mutations during iteration.
#[tokio::test]
async fn mare_g_rejects_cycles_and_pairs_returns_isolated_snapshot() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (cyc_ok, cyc_err, fn_ok, snap_ok, live_ok): (bool, String, bool, bool, bool) = eval(
            &h,
            r#"
            local a = { x = 1 }
            a.self = a
            local cyc_ok, cyc_err = pcall(function() mare.g.cyc = a end)
            local fn_ok = pcall(function() mare.g.bad = function() end end)

            mare.g.alpha = { count = 1 }
            mare.g.beta = "two"
            local seen = {}
            for k, v in pairs(mare.g) do
                seen[k] = v
                if type(v) == "table" then
                    v.count = 999
                end
                mare.g.gamma = "added-during-iter"
            end
            local snap_ok = seen.alpha ~= nil and seen.beta == "two" and seen.gamma == nil
            local live_ok = mare.g.alpha.count == 1 and mare.g.gamma == "added-during-iter"
            return cyc_ok, tostring(cyc_err), fn_ok, snap_ok, live_ok
            "#,
        )
        .await;
        assert!(!cyc_ok && cyc_err.contains("cycles"), "cyclic table must be rejected: {cyc_err}");
        assert!(!fn_ok, "functions must not be stored in mare.g");
        assert!(snap_ok, "pairs(mare.g) must iterate an isolated snapshot");
        assert!(live_ok, "mutating yielded copy must not affect stored mare.g value");
        h.shutdown().await;
    })
    .await
}
