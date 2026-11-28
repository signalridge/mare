use std::process::Stdio;

use crate::common::*;

/// Codex tui_keymap.rs:814 tests that a shortcut can be customized or disabled.
#[tokio::test]
async fn agents_open_key_uses_default_keymap_and_can_be_rebound_or_disabled() {
    local(async {
        for (name, extra_lua, expected) in [
            ("disabled", r#"mare.keymap.set("editor", "<Left>", false)"#, None),
            (
                "rebound",
                r#"mare.keymap.set("editor", "<Left>", false); mare.keymap.set("editor", "<F8>", "agents")"#,
                Some("f8"),
            ),
            ("default", "", Some("left")),
        ] {
            let env = TestEnv::new().await;
            std::fs::create_dir_all(env.config_dir()).unwrap();
            std::fs::write(env.config_dir().join("init.lua"), extra_lua).unwrap();
            let h = env.host().builtins(&["agents"]).user_config(true).no_session().build().await;
            let (active, has_keys_opt, keys): (bool, bool, Vec<String>) = eval(
                &h,
                r#"local keys = {}
                   for _, binding in ipairs(mare.keymap.list()) do
                       if binding.action == "agents" and binding.context == "editor" then
                           keys[#keys + 1] = binding.key
                       end
                   end
                   local has_keys_opt = false
                   for _, k in ipairs(mare.config_declared()) do
                       if k == "plugins.agents.keys" or k:sub(1, 20) == "plugins.agents.keys." then
                           has_keys_opt = true
                       end
                   end
                   return mare.plugins.status("agents").active == true, has_keys_opt, keys"#,
            )
            .await;
            assert!(active, "agents must start with the {name} key configuration");
            assert!(!has_keys_opt, "agents must no longer expose `keys` in config_schema");
            assert_eq!(keys, expected.into_iter().map(str::to_owned).collect::<Vec<_>>(), "{name}");
            h.shutdown().await;
        }
    })
    .await;
}

#[tokio::test]
async fn provider_empty_oauth_key_is_rejected_without_affecting_valid_providers() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins/oauth-key-probe");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("init.lua"),
            r#"return {
              name = "oauth-key-probe", inject = { "llm" },
              providers = { "empty-oauth", "valid-oauth", "without-oauth" },
              apply = function(ctx)
                ctx.llm:register_provider({ provider = "empty-oauth", oauth_key = "" })
                ctx.llm:register_provider({ provider = "valid-oauth", oauth_key = "valid-oauth-key" })
                ctx.llm:register_provider({ provider = "without-oauth" })
              end,
            }"#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        let (bad_absent, valid_key, no_oauth, status): (bool, String, bool, String) = eval(
            &h,
            r#"local valid = LLM:provider("valid-oauth")
               local without = LLM:provider("without-oauth")
               return LLM:provider("empty-oauth") == nil,
                   valid and valid.oauth_key or "missing",
                   without ~= nil and without.oauth_key == nil,
                   mare.json.encode(mare.plugins.status("oauth-key-probe"))"#,
        )
        .await;
        h.shutdown().await;

        let status: serde_json::Value = serde_json::from_str(&status).unwrap();
        let problems = status["problems"].as_array().unwrap();
        assert!(bad_absent, "provider with empty oauth_key must not register: {status}");
        assert_eq!(valid_key, "valid-oauth-key");
        assert!(no_oauth, "providers without OAuth remain valid");
        assert!(
            problems.iter().any(|problem| {
                problem["key"] == "provider:empty-oauth"
                    && problem["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("oauth_key") && message.contains("non-empty"))
            }),
            "missing actionable oauth_key declaration problem: {status}"
        );
    })
    .await;
}

#[tokio::test]
async fn third_party_tool_uses_the_guarded_entry_and_persistent_agent_contract() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins/contract-probe");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("init.lua"),
            r#"return { name = "contract-probe", inject = { "tools", "policy" }, apply = function(ctx)
                 ctx.tools:register("contract_write", {
                   description = "write a contract probe file", side_effect = "write",
                   parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
                   execute = function(a) assert(mare.fs.write(a.path, "plugin result")) return "written" end,
                 })
                 ctx.policy:voter("tool/before", function(call)
                   if call.name == "contract_write" and call.arguments.path == "blocked.txt" then
                     return { decision = "deny", reason = "plugin rule" }
                   end
                 end)
               end }"#,
        )
        .unwrap();
        let script = serde_json::json!([
            { "tool_calls": [
                { "name": "contract_write", "arguments": { "path": "allowed.txt" } },
                { "name": "contract_write", "arguments": { "path": "blocked.txt" } }
            ] },
            { "text": "done" }
        ]);
        let h = env.host().faux(script).user_plugins().build().await;
        expose(&h);
        let result = h.prompt("exercise plugin", serde_json::json!({})).await.unwrap();
        assert_eq!(result["text"], "done");
        assert_eq!(std::fs::read_to_string(env.cwd.join("allowed.txt")).unwrap(), "plugin result");
        assert!(!env.cwd.join("blocked.txt").exists());
        let path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;
        let records: Vec<serde_json::Value> =
            std::fs::read_to_string(path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let states: Vec<_> = records
            .iter()
            .filter(|r| r["type"] == "tool_state" && r["tool_call_id"] == "call_1_1")
            .filter_map(|r| r["state"].as_str())
            .collect();
        assert_eq!(states, ["prepared", "started", "completed"]);
        assert!(records.iter().any(|r| r["type"] == "message" && r["message"]["tool_call_id"] == "call_1_1"));
    })
    .await
}

// guarantee: reload-rereads-plugin-modules
#[tokio::test]
async fn reload_picks_up_edited_lua_modules() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins/review");
        std::fs::create_dir_all(pdir.join("lua/review")).unwrap();
        std::fs::write(
            pdir.join("init.lua"),
            r#"return { name = "review", override = true, provide = { "review" }, apply = function(ctx)
                 require("mare_lib").service(ctx, "review", { run = function() return require("review.run").go() end, viewer = function() end })
               end }"#,
        )
        .unwrap();
        std::fs::write(pdir.join("lua/review/run.lua"), r#"return { go = function() return "v1" end }"#).unwrap();
        let h = env.host().builtins(&["review"]).user_plugins().build().await;
        global(&h, "R", h.kernel.service("review").unwrap());
        assert_eq!(eval::<String>(&h, "return R:run()").await, "v1");
        // a different mtime even on coarse filesystems
        let f = pdir.join("lua/review/run.lua");
        std::fs::write(&f, r#"return { go = function() return "v2!" end }"#).unwrap();
        let t = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(t).unwrap();
        h.kernel.reload(Some("review")).await.unwrap();
        global(&h, "R", h.kernel.service("review").unwrap());
        assert_eq!(eval::<String>(&h, "return R:run()").await, "v2!");
        h.shutdown().await;
    })
    .await
}

/// Write a user plugin file with an mtime that differs even on coarse filesystems (the bytecode
/// cache keys on it).
fn write_bumped(path: &std::path::Path, code: &str, secs: u64) {
    std::fs::write(path, code).unwrap();
    let t = std::time::SystemTime::now() + std::time::Duration::from_secs(secs);
    std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
}

const COUNTER: &str = r#"return { name = "rel", provide = { "rel" }, contracts = { rel = { methods = { v = { doc = "v", result = { type = "string" } } } } }, apply = function(ctx)
    mare._applies = (mare._applies or 0) + 1
    require("mare_lib").service(ctx, "rel", { v = function() return "v1" end })
  end }"#;

#[tokio::test]
async fn required_modules_run_in_their_plugin_environment() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins/envp");
        std::fs::create_dir_all(pdir.join("lua/envp")).unwrap();
        std::fs::write(
            pdir.join("init.lua"),
            r#"return { name = "envp", provide = { "envp" }, contracts = { envp = { methods = { get = { doc = "get", result = { type = "string" } } } } }, apply = function(ctx)
                 PLUGIN_ONLY = "yes" -- a top-level global write is refused
                 require("mare_lib").service(ctx, "envp", { get = function() return require("envp.m").get() end })
               end }"#,
        )
        .unwrap();
        std::fs::write(
            pdir.join("lua/envp/m.lua"),
            r#"LEAKED_FROM_MODULE = true
               return { get = function() return PLUGIN_ONLY end }"#,
        )
        .unwrap();
        let h = env.host().user_plugins().build().await;
        global(&h, "E", h.kernel.service("envp").unwrap());
        assert_eq!(eval::<Option<String>>(&h, "return E:get()").await.as_deref(), Some("yes"));
        let leaked: bool = eval(&h, "return rawget(_G, 'LEAKED_FROM_MODULE') ~= nil").await;
        assert!(!leaked, "a required module's globals stay in its plugin's environment");
        h.shutdown().await;
    })
    .await
}

// guarantee: broken-definition-reload-changes-nothing
#[tokio::test]
async fn reload_with_a_broken_definition_changes_nothing() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        let f = pdir.join("rel.lua");
        std::fs::write(&f, COUNTER).unwrap();
        let h = env.host().user_plugins().build().await;
        assert_eq!(eval::<i64>(&h, "return mare._applies").await, 1);
        write_bumped(&f, "return { name = 'rel', apply = function( end }", 2);
        let err = h.kernel.reload(Some("rel")).await.unwrap_err().to_string();
        assert!(err.contains("nothing changed"), "{err}");
        // the running instance was never disposed or re-applied
        assert_eq!(eval::<i64>(&h, "return mare._applies").await, 1);
        assert!(crate::common::plugin_active(&h.kernel, "rel"));
        global(&h, "S", h.kernel.service("rel").unwrap());
        assert_eq!(eval::<String>(&h, "return S:v()").await, "v1");
        h.shutdown().await;
    })
    .await
}

// guarantee: failed-reload-keeps-the-old-definition-serving
#[tokio::test]
async fn reload_whose_apply_fails_reenables_the_previous_definition() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        let f = pdir.join("rel.lua");
        std::fs::write(&f, COUNTER).unwrap();
        let h = env.host().user_plugins().build().await;
        write_bumped(&f, r#"return { name = "rel", provide = { "rel" }, contracts = { rel = { methods = { v = { doc = "v", result = { type = "string" } } } } }, apply = function() error("boom") end }"#, 2);
        let applied = h.kernel.reload(Some("rel")).await.unwrap();
        assert_eq!(applied, vec!["rel".to_string()]);
        // the old definition came back as a fresh instance (applied a second time)
        assert_eq!(eval::<i64>(&h, "return mare._applies").await, 2);
        global(&h, "S", h.kernel.service("rel").unwrap());
        assert_eq!(eval::<String>(&h, "return S:v()").await, "v1");
        let problems = h.kernel.problems().join("\n");
        assert!(problems.contains("boom") && problems.contains("re-enabled"), "{problems}");
        h.shutdown().await;
    })
    .await
}

/// (WezTerm keeps the last good config): a full /reload whose new definition fails
/// in `apply` keeps the plugin running on its previous definition and reports the error.
#[tokio::test]
async fn full_reload_whose_apply_fails_keeps_the_plugin_running() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        let f = pdir.join("rel.lua");
        std::fs::write(&f, COUNTER).unwrap();
        let h = env.host().user_plugins().build().await;
        write_bumped(&f, r#"return { name = "rel", provide = { "rel" }, contracts = { rel = { methods = { v = { doc = "v", result = { type = "string" } } } } }, apply = function() error("boom") end }"#, 2);
        let out = h.reload_all().await.unwrap();
        assert!(out["notices"].to_string().contains("boom"), "{out}");
        assert!(crate::common::plugin_active(&h.kernel, "rel"), "never left unloaded");
        global(&h, "S", h.kernel.service("rel").unwrap());
        assert_eq!(eval::<String>(&h, "return S:v()").await, "v1");
        h.shutdown().await;
    })
    .await
}

/// a handler that keeps failing is notified every time (shown as a
/// hook error under every prompt), the plugin stays loaded, and its latest error stays in the
/// problems (one line per handler) until a full /reload.
// guarantee: handler-error-reported-and-plugin-keeps-running
#[tokio::test]
async fn a_failing_handler_is_notified_every_time() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("flaky.lua"),
            r#"return { name = "flaky", apply = function(ctx)
                 ctx:on("probe/ping", function() mare._pings = (mare._pings or 0) + 1; error("handler boom") end)
               end }"#,
        )
        .unwrap();
        let h = env.host().user_plugins().build().await;
        expose(&h);
        exec(&h, r#"mare._errors = 0; C.cordis:on("kernel/error", function() mare._errors = mare._errors + 1 end)"#)
            .await;
        for _ in 0..3 {
            h.kernel.emit("probe/ping", mlua::MultiValue::new()).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(eval::<(i64, i64)>(&h, "return mare._pings, mare._errors").await, (3, 3), "every time");
        assert!(crate::common::plugin_active(&h.kernel, "flaky"), "the plugin is not disabled");
        let problems = h.kernel.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with("plugin flaky: listener for `probe/ping` failed: ")
                && problems[0].contains("handler boom"),
            "{problems:?}"
        );
        h.reload_all().await.unwrap();
        assert!(h.kernel.problems().is_empty(), "a full reload clears it: {:?}", h.kernel.problems());
        h.shutdown().await;
    })
    .await
}

// guarantee: cancelled-reload-waits-for-disposal
#[tokio::test]
async fn cancelled_reload_waits_for_disposal_before_retrying() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        let plugin = pdir.join("dispose_probe.lua");
        std::fs::write(
            &plugin,
            r#"return {name='dispose_probe', apply=function(ctx)
          mare._dispose_applied=(mare._dispose_applied or 0)+1
          ctx:effect(function() return function() mare._dispose_started=true; for _ = 1, 12000 do if mare._dispose_release then break end mare.task.sleep(5) end; mare._dispose_done=true end end)
        end}"#,
        )
        .unwrap();
        let h = env.host().user_plugins().no_session().build().await;
        std::fs::write(
            &plugin,
            r#"return {name='dispose_probe', apply=function() mare._dispose_reapplied=mare._dispose_done == true end}"#,
        )
        .unwrap();
        // cancel the reload once the old plugin's disposal is running (parked until released below)
        tokio::select! {
            r = h.kernel.reload(Some("dispose_probe")) => panic!("reload unexpectedly completed: {r:?}"),
            () = wait_until(WAIT_SECS, "the disposal to start", || {
                let h = h.clone();
                async move { eval::<bool>(&h, "return mare._dispose_started == true").await }
            }) => {}
        }
        assert!(h.kernel.reload_incomplete().contains(&"dispose_probe".to_string()));
        exec(&h, "mare._dispose_release = true").await;
        h.kernel.reload(Some("dispose_probe")).await.unwrap();
        assert!(eval::<bool>(&h, "return mare._dispose_reapplied == true").await);
        assert!(h.kernel.reload_incomplete().is_empty());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn large_file_reads_do_not_block_timers() {
    local(async {
        let env = TestEnv::new().await;
        let big = env.cwd.join("big.bin");
        std::fs::write(&big, vec![b'x'; 128 << 20]).unwrap();
        let h = env.host().build().await;
        global(&h, "BIG", big.to_string_lossy().to_string());
        let (gap, took): (f64, f64) = eval(
            &h,
            r#"
            local done, gap = false, 0
            mare.task.spawn(function()
                while not done do
                    local t = mare.task.clock()
                    mare.task.sleep(10)
                    gap = math.max(gap, mare.task.clock() - t)
                end
            end)
            mare.task.sleep(30)
            local t0 = mare.task.clock()
            local s = mare.fs.read(BIG)
            local took = mare.task.clock() - t0
            assert(#s == 128 * 1024 * 1024)
            mare.task.sleep(30)
            done = true
            return gap, took
        "#,
        )
        .await;
        // a blocking read would hold the timer for the whole read
        if took > 40.0 {
            assert!(gap < took * 0.75, "timer gap {gap:.0}ms during a {took:.0}ms read");
        }
        h.shutdown().await;
    })
    .await
}

/// `llm` defers cold request/error/auth/credential submodules out of `apply` and loads them on
/// first use while preserving published service method behavior and monkey-patch seams.
#[tokio::test]
async fn llm_apply_defers_cold_submodules_until_first_use() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&[]).no_session().build().await;

        let cold_modules = [
            "llm.errors",
            "llm.history",
            "llm.send",
            "llm.api_errors",
            "llm.tool_constraints",
            "llm.stream",
            "llm.projection",
            "llm.auth_store",
            "llm.credentials",
        ];
        for module in cold_modules {
            assert!(
                !h.kernel.is_module_loaded("llm", module),
                "cold module `{module}` must not be loaded during llm apply"
            );
        }

        global(&h, "L", h.service_table("llm").unwrap());

        // Fast-path queries before any stream starts must not force llm.stream to load.
        let pending: bool = eval(&h, r#"return L:side_requests_pending("no-such-owner")"#).await;
        assert!(!pending);
        exec(&h, r#"L:cancel_side_requests("no-such-owner")"#).await;
        assert!(
            !h.kernel.is_module_loaded("llm", "llm.stream"),
            "side_requests_pending/cancel_side_requests must not eagerly load llm.stream when idle"
        );

        // Monkey-patching a lazily loaded method BEFORE it loads must survive lazy loading and
        // still intercept internal calls (`complete` -> `self:stream`) after `llm.stream` loads.
        let pre_load_patched: String = eval(
            &h,
            r#"local orig = L.stream
               L.stream = function(_, _req, _sink)
                   return { role = "assistant", content = { { type = "text", text = "pre-load-patched" } } }
               end
               local msg = L:complete({ model = L:resolve("faux/echo"), messages = {} })
               L.stream = orig
               return msg.content[1].text"#,
        )
        .await;
        assert_eq!(pre_load_patched, "pre-load-patched");
        assert!(h.kernel.is_module_loaded("llm", "llm.stream"));
        assert!(h.kernel.is_module_loaded("llm", "llm.projection"));
        assert!(h.kernel.is_module_loaded("llm", "llm.errors"));
        assert!(h.kernel.is_module_loaded("llm", "llm.history"));

        let txt: String = eval(&h, r#"return L:text_of({ content = { { type = "text", text = "hello" } } })"#).await;
        assert_eq!(txt, "hello");

        let kind: String = eval(&h, r#"local k = L:classify("429 rate limit exceeded"); return tostring(k)"#).await;
        assert!(!kind.is_empty());
        assert!(h.kernel.is_module_loaded("llm", "llm.api_errors"));

        let constrained: bool =
            eval(&h, r#"local r = L:constrain_request({ tools = {} }); return r._tools_constrained == true"#).await;
        assert!(constrained);
        assert!(h.kernel.is_module_loaded("llm", "llm.tool_constraints"));

        let missing_auth: bool = eval(&h, r#"return L:auth_get("nonexistent-provider") == nil"#).await;
        assert!(missing_auth);
        assert!(h.kernel.is_module_loaded("llm", "llm.auth_store"));

        let _has_creds: bool = eval(&h, r#"return L:has_credentials()"#).await;
        assert!(h.kernel.is_module_loaded("llm", "llm.credentials"));

        // Monkey-patching a lazily loaded service method after load still intercepts calls cleanly.
        let patched: String =
            eval(&h, r#"L.text_of = function() return "patched" end; return L:text_of({ content = {} })"#).await;
        assert_eq!(patched, "patched");

        h.shutdown().await;
    })
    .await
}

/// `provider-openai` defers its wire, auth, login, and websocket submodules out of `apply` and
/// loads each only when its path is first invoked.
#[tokio::test]
async fn provider_openai_apply_defers_cold_submodules_until_first_use() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&[]).no_session().build().await;

        let cold_modules = [
            "provider-openai.wire",
            "provider-openai.auth",
            "provider-openai.login",
            "provider-openai.websocket",
            "provider-openai.codex_models",
        ];
        for module in cold_modules {
            assert!(
                !h.kernel.is_module_loaded("provider-openai", module),
                "cold module `{module}` must not be loaded during provider-openai apply"
            );
        }

        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "BUS", h.service_table("bus").unwrap());

        // Wire metadata and provider registration are available immediately without loading wire.lua.
        let wire_route: String = eval(&h, r#"return L:describe_wire("openai-responses").route"#).await;
        assert_eq!(wire_route, "responses");
        let is_img_limit: bool =
            eval(&h, r#"return L:provider("openai").errors.image_limit("too many images", nil) == true"#).await;
        assert!(is_img_limit);
        assert!(!h.kernel.is_module_loaded("provider-openai", "provider-openai.wire"));

        // Emitting a real non-OpenAI `provider/transport` event must return nil without loading websocket.lua.
        let non_openai_res: bool = eval(
            &h,
            r#"return BUS:bail("provider/transport", {
                   provider = "anthropic",
                   request = { url = "https://api.anthropic.com/v1/messages" },
               }) == nil"#,
        )
        .await;
        assert!(non_openai_res);
        assert!(!h.kernel.is_module_loaded("provider-openai", "provider-openai.websocket"));

        // Auth, login, and wire each load lazily on first use.
        let _configured: bool = eval(&h, r#"return L:provider("openai").is_configured()"#).await;
        assert!(h.kernel.is_module_loaded("provider-openai", "provider-openai.auth"));
        assert!(!h.kernel.is_module_loaded("provider-openai", "provider-openai.login"));
        assert!(!h.kernel.is_module_loaded("provider-openai", "provider-openai.wire"));

        exec(&h, r#"L:provider("openai").revoke("other-key", {})"#).await;
        assert!(h.kernel.is_module_loaded("provider-openai", "provider-openai.login"));

        exec(
            &h,
            r#"pcall(L:wire("openai-responses"), {
                   provider = "openai",
                   label = "OpenAI",
                   url = "http://127.0.0.1:1/responses",
                   key = "sk-test",
               }, {
                   model = L:resolve("openai/gpt-6.1-terra"),
                   system = "",
                   messages = {},
               }, function() end)"#,
        )
        .await;
        assert!(h.kernel.is_module_loaded("provider-openai", "provider-openai.wire"));

        h.shutdown().await;
    })
    .await
}

/// `provider-anthropic`, `provider-google`, and `provider-openai-compat` defer their wire/stream
/// submodules out of `apply` and load each only on first wire use or error classification.
#[tokio::test]
async fn provider_anthropic_google_and_compat_apply_defer_wire_and_stream_until_first_use() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&[]).no_session().build().await;

        assert!(
            !h.kernel.is_module_loaded("provider-anthropic", "provider-anthropic.wire"),
            "provider-anthropic.wire must not load during apply"
        );
        assert!(
            !h.kernel.is_module_loaded("provider-anthropic", "provider-anthropic.stream"),
            "provider-anthropic.stream must not load during apply"
        );
        assert!(
            !h.kernel.is_module_loaded("provider-google", "provider-google.wire"),
            "provider-google.wire must not load during apply"
        );
        assert!(
            !h.kernel.is_module_loaded("provider-google", "provider-google.stream"),
            "provider-google.stream must not load during apply"
        );
        assert!(
            !h.kernel.is_module_loaded("provider-openai-compat", "provider-openai-compat.wire"),
            "provider-openai-compat.wire must not load during apply"
        );

        global(&h, "L", h.service_table("llm").unwrap());

        // Wire metadata and preset model resolution work immediately without loading wire/stream.
        let (a_route, g_route, c_route, ds_ok): (String, String, String, bool) = eval(
            &h,
            r#"return L:describe_wire("anthropic-messages").route,
                      L:describe_wire("google-generate").route,
                      L:describe_wire("openai-chat").route,
                      L:resolve("deepseek/deepseek-flash") ~= nil"#,
        )
        .await;
        assert_eq!(a_route, "anthropic");
        assert_eq!(g_route, "google");
        assert_eq!(c_route, "chat");
        assert!(ds_ok);
        assert!(!h.kernel.is_module_loaded("provider-anthropic", "provider-anthropic.wire"));
        assert!(!h.kernel.is_module_loaded("provider-anthropic", "provider-anthropic.stream"));
        assert!(!h.kernel.is_module_loaded("provider-google", "provider-google.wire"));
        assert!(!h.kernel.is_module_loaded("provider-google", "provider-google.stream"));
        assert!(!h.kernel.is_module_loaded("provider-openai-compat", "provider-openai-compat.wire"));

        // First wire call loads each provider's wire/stream lazily.
        exec(
            &h,
            r#"pcall(L:wire("anthropic-messages"), {
                   provider = "anthropic",
                   label = "Anthropic",
                   url = "http://127.0.0.1:1/v1/messages",
                   key = "sk-ant-test",
               }, {
                   model = L:resolve("anthropic/claude-sonnet-5-5"),
                   system = "",
                   messages = {},
               }, function() end)"#,
        )
        .await;
        assert!(h.kernel.is_module_loaded("provider-anthropic", "provider-anthropic.wire"));
        assert!(h.kernel.is_module_loaded("provider-anthropic", "provider-anthropic.stream"));
        assert!(!h.kernel.is_module_loaded("provider-google", "provider-google.wire"));
        assert!(!h.kernel.is_module_loaded("provider-openai-compat", "provider-openai-compat.wire"));

        exec(
            &h,
            r#"pcall(L:wire("google-generate"), {
                   provider = "google",
                   label = "Google",
                   base_url = "http://127.0.0.1:1/v1beta",
                   key = "AIza-test",
               }, {
                   model = L:resolve("google/gemini-2.5-pro"),
                   system = "",
                   messages = {},
               }, function() end)"#,
        )
        .await;
        assert!(h.kernel.is_module_loaded("provider-google", "provider-google.wire"));
        assert!(h.kernel.is_module_loaded("provider-google", "provider-google.stream"));
        assert!(!h.kernel.is_module_loaded("provider-openai-compat", "provider-openai-compat.wire"));

        exec(
            &h,
            r#"pcall(L:wire("openai-chat"), {
                   provider = "kimi",
                   label = "Kimi",
                   url = "http://127.0.0.1:1/chat/completions",
                   key = "sk-kimi-test",
               }, {
                   model = L:resolve("kimi/kimi-k3.1-pro"),
                   system = "",
                   messages = {},
               }, function() end)"#,
        )
        .await;
        assert!(h.kernel.is_module_loaded("provider-openai-compat", "provider-openai-compat.wire"));

        h.shutdown().await;
    })
    .await
}

/// Built-in provider plugins register synchronously without reading disk catalogs or `auth.json`
/// during `apply`, and the request preparation barrier hydrates disk-cached model metadata before
/// the first request runs. `provider-faux` also defers `MARE_FAUX_SCRIPT` disk reads out of `apply`.
#[tokio::test]
async fn provider_plugins_apply_synchronously_and_hydrate_catalog_in_request_barrier() {
    local(async {
        let env = TestEnv::new().await;
        let auth_path = env.config_dir().join("auth.json");
        std::fs::create_dir_all(auth_path.parent().unwrap()).unwrap();
        std::fs::write(
            &auth_path,
            r#"{"version":1,"anthropic":{"type":"api_key","key":"sk-ant-test"},"openai":{"type":"api_key","key":"sk-test"},"google":{"type":"api_key","key":"AIza-test"},"opencode-go":{"type":"api_key","key":"sk-op-test"},"deepseek":{"type":"api_key","key":"sk-ds-test"}}"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let cache_dir = env.home.path().join("state/models-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            cache_dir.join("anthropic.json"),
            r#"{"fetched_at":9999999999,"models":[{"id":"cached-barrier-model","name":"Cached Barrier","context_window":333000,"max_output":12345,"reasoning":true}]}"#,
        )
        .unwrap();

        mare_shared::profile::enable_startup_profile();
        let before_len = mare_shared::profile::entries().len();
        let h = env.host().builtins(&[]).no_session().build().await;
        let entries = mare_shared::profile::entries();
        let recent = &entries[before_len..];

        for plugin in [
            "llm",
            "provider-anthropic",
            "provider-google",
            "provider-openai",
            "provider-openai-compat",
            "provider-opencode-go",
            "provider-faux",
        ] {
            let row = recent
                .iter()
                .rev()
                .find(|r| r.label.starts_with(&format!("plugin {plugin} (builtin)")))
                .unwrap_or_else(|| panic!("missing profile entry for {plugin}: {recent:#?}"));
            assert!(
                !row.label.contains("yielded"),
                "{plugin} must apply synchronously without disk I/O yields, got: {}",
                row.label
            );
        }

        global(&h, "L", h.service_table("llm").unwrap());

        // Before the request barrier runs, disk-cached model metadata was not read during apply.
        let ctx_before: i64 =
            eval(&h, r#"return L:resolve("anthropic/cached-barrier-model").context_window"#).await;
        assert_eq!(ctx_before, 200000, "catalog cache must stay unread during apply");

        // Request preparation barrier hydrates the selected provider's disk catalog synchronously.
        let (ctx_after, max_out_after): (i64, i64) = eval(
            &h,
            r#"local m = L:resolve("anthropic/cached-barrier-model")
               local ev = { model = m }
               L:prepare_request(ev)
               local hydrated = ev.model or L:resolve("anthropic/cached-barrier-model")
               return hydrated.context_window, hydrated.max_output"#,
        )
        .await;
        assert_eq!(ctx_after, 333000, "request barrier must hydrate cached context_window");
        assert_eq!(max_out_after, 12345, "request barrier must hydrate cached max_output");

        h.shutdown().await;

        // `provider-faux` must not read `MARE_FAUX_SCRIPT` during apply: an unreadable path must
        // not fail plugin activation during `mare doctor`, while a valid path is read on first stream.
        let missing_path = env.home.path().join("does-not-exist-faux.json");
        let doctor_out = mare_cmd()
            .args(["doctor", "--json"])
            .env("MARE_HOME", env.home.path())
            .env("MARE_FAUX_SCRIPT", &missing_path)
            .current_dir(&env.cwd)
            .stdin(Stdio::null())
            .run_output();
        let doctor_text = String::from_utf8_lossy(&doctor_out.stdout);
        let doctor_json: serde_json::Value =
            serde_json::from_str(&doctor_text).unwrap_or_else(|e| panic!("{e}: {doctor_text}"));
        let checks = doctor_json["checks"].as_array().unwrap_or_else(|| panic!("{doctor_text}"));
        assert!(
            !checks.iter().any(|c| {
                c["ok"] == false && c["detail"].as_str().is_some_and(|d| d.contains("provider-faux"))
            }),
            "provider-faux must not read MARE_FAUX_SCRIPT during apply:\n{doctor_text}"
        );

        // Missing or malformed MARE_FAUX_SCRIPT reports a clear error with the file path on first request.
        let missing_req = mare_cmd()
            .args(["--no-session", "--model", "faux/echo", "-p", "hello"])
            .env("MARE_HOME", env.home.path())
            .env("MARE_FAUX_SCRIPT", &missing_path)
            .current_dir(&env.cwd)
            .stdin(Stdio::null())
            .run_output();
        assert!(!missing_req.status.success(), "missing MARE_FAUX_SCRIPT must fail on first request");
        let missing_err = format!(
            "{}\n{}",
            String::from_utf8_lossy(&missing_req.stdout),
            String::from_utf8_lossy(&missing_req.stderr)
        );
        assert!(
            missing_err.contains("MARE_FAUX_SCRIPT")
                && missing_err.contains(&missing_path.display().to_string()),
            "missing MARE_FAUX_SCRIPT error must name the variable and file path:\n{missing_err}"
        );

        let bad_script_path = env.home.path().join("faux-malformed.json");
        std::fs::write(&bad_script_path, "{not a json array").unwrap();
        let bad_req = mare_cmd()
            .args(["--no-session", "--model", "faux/echo", "-p", "hello"])
            .env("MARE_HOME", env.home.path())
            .env("MARE_FAUX_SCRIPT", &bad_script_path)
            .current_dir(&env.cwd)
            .stdin(Stdio::null())
            .run_output();
        assert!(!bad_req.status.success(), "malformed MARE_FAUX_SCRIPT must fail on first request");
        let bad_err = format!(
            "{}\n{}",
            String::from_utf8_lossy(&bad_req.stdout),
            String::from_utf8_lossy(&bad_req.stderr)
        );
        assert!(
            bad_err.contains("MARE_FAUX_SCRIPT")
                && bad_err.contains(&bad_script_path.display().to_string()),
            "malformed MARE_FAUX_SCRIPT error must name the variable and file path:\n{bad_err}"
        );

        let script_path = env.home.path().join("faux-lazy.json");
        std::fs::write(&script_path, r#"[{"text":"lazy faux reply"}]"#).unwrap();
        let prompt_out = mare_cmd()
            .args(["--no-session", "--model", "faux/echo", "-p", "hello"])
            .env("MARE_HOME", env.home.path())
            .env("MARE_FAUX_SCRIPT", &script_path)
            .current_dir(&env.cwd)
            .stdin(Stdio::null())
            .run_output();
        assert!(
            prompt_out.status.success(),
            "lazy MARE_FAUX_SCRIPT stream failed: {}",
            String::from_utf8_lossy(&prompt_out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&prompt_out.stdout).contains("lazy faux reply"),
            "unexpected stdout: {}",
            String::from_utf8_lossy(&prompt_out.stdout)
        );
    })
    .await
}
