//! The readiness contract's decided cases: a provider's `prepare`
//! retrying a temporary failure (cancellable), a disabled provider on resume, and a plugin that
//! replaces a core service without declaring it.

use serde_json::json;

use crate::common::*;

mod selected_prepare;

/// The `request/ready` deny reason, or None when requests may run.
async fn blocked_reason(h: &mare_host::Host) -> Option<String> {
    let verdict = h.kernel.policy("request/ready", mlua::MultiValue::new()).await.unwrap();
    let (decision, reason): (String, Option<String>) =
        (verdict.get("decision").unwrap(), verdict.get("reason").unwrap());
    (decision == "deny").then(|| reason.unwrap_or_default())
}

/// Give the faux provider a `prepare` that raises `err` (a Lua expression) on the calls listed in
/// `fail_on` (`true`: every call), counting calls in `PREPARES`.
async fn stub_prepare(h: &mare_host::Host, err: &str, fail_on: &str) {
    global(h, "L", h.service_table("llm").unwrap());
    exec(
        h,
        &format!(
            "PREPARES = 0
             L:provider('faux').prepare = function()
               PREPARES = PREPARES + 1
               local fail = {fail_on}
               if fail == true or fail[PREPARES] then error({err}, 0) end
             end"
        ),
    )
    .await;
}

/// A temporary refresh failure is tried again before the prompt (Kimi oauth.ts retries network,
/// 429 and 5xx): once it succeeds the prompt goes; when it keeps failing the prompt still goes
/// (the request uses the cached token, Codex manager.rs:2036-2041) after 2 retries.
#[tokio::test]
async fn a_temporary_prepare_failure_is_retried_then_the_prompt_goes() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "one" }, { "text": "two" }])).no_session().build().await;
        let transient = "{ message = 'refresh failed (HTTP 503)', kind = 'server', retryable = true }";
        stub_prepare(&h, transient, "{ true }").await;
        let prompt = h.prompt("hello", json!({})).await.unwrap();
        assert!(prompt["error"].is_null(), "{prompt}");
        assert_eq!(eval::<i64>(&h, "return PREPARES").await, 2, "failed once, then succeeded");

        stub_prepare(&h, transient, "true").await;
        let prompt = h.prompt("hello", json!({})).await.unwrap();
        assert!(prompt["error"].is_null(), "a temporary failure never refuses: {prompt}");
        assert_eq!(prompt["text"], "two");
        assert_eq!(eval::<i64>(&h, "return PREPARES").await, 3, "the first try and 2 retries");
        h.shutdown().await;
    })
    .await
}

/// A failure needing a new login (401, refresh token expired/reused/revoked) is not retried: the
/// prompt is "not sent", saying how to log in (Codex manager.rs:1366-1374).
#[tokio::test]
async fn a_permanent_prepare_failure_refuses_without_retrying() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "must not run" }])).no_session().build().await;
        stub_prepare(&h, "{ message = 'refresh token was revoked', kind = 'auth' }", "true").await;
        let prompt = h.prompt("hello", json!({})).await.unwrap();
        assert_eq!(prompt["not_sent"], true, "{prompt}");
        let error = prompt["error"]["message"].as_str().unwrap();
        assert!(error.starts_with("model faux/echo: refresh token was revoked — "), "{error}");
        assert!(error.contains("mare login faux"), "says how to log in: {error}");
        assert_eq!(eval::<i64>(&h, "return PREPARES").await, 1, "not retried");
        h.shutdown().await;
    })
    .await
}

/// Esc during the backoff ends the wait at once: the prompt stops as cancelled, nothing is built
/// or sent, and no further attempt is made.
#[tokio::test]
async fn cancelling_during_the_prepare_backoff_stops_at_once() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "must not run" }])).no_session().build().await;
        expose(&h);
        // The server asks for 30 s between tries. llm/prepare only honours a wait up to MAX_WAIT_MS
        // (2 s) within BUDGET_MS (5 s), so lift both for this test: the abort then lands inside the
        // first backoff however slow the runner is, and a backoff that ignored the cancel would be
        // caught by the latency bound below.
        exec(&h, "local p = require('llm.prepare'); p.MAX_WAIT_MS, p.BUDGET_MS = 600000, 600000").await;
        let slow = "{ message = 'rate limited', kind = 'rate_limit', retryable = true, retry_after_ms = 30000 }";
        stub_prepare(&h, slow, "true").await;
        exec(&h, "REQUEST_BUILDS = 0; C.cordis:on('request/build', function(req, next) REQUEST_BUILDS = REQUEST_BUILDS + 1; return next() end)").await;
        let worker = h.clone();
        let prompt = tokio::task::spawn_local(async move { worker.prompt("cancel me", json!({})).await });
        wait_until(WAIT_SECS, "the first prepare", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return PREPARES == 1 and A:get().busy").await }
        })
        .await;
        // the ceiling only guards a hang; "at once" is the separate latency bound below
        let aborted_at = std::time::Instant::now();
        h.abort().await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), prompt)
            .await
            .expect("Esc did not end the backoff")
            .unwrap()
            .unwrap();
        let stop = aborted_at.elapsed();
        assert!(stop < std::time::Duration::from_secs(5), "the backoff took {stop:?} to stop after the abort");
        assert!(result["error"]["message"].as_str().is_some_and(|e| e.contains("abort")), "{result}");
        assert!(result["not_sent"].is_null(), "cancelled, not refused: {result}");
        assert_eq!(eval::<i64>(&h, "return PREPARES").await, 1, "no attempt after the cancel");
        assert_eq!(eval::<i64>(&h, "return REQUEST_BUILDS").await, 0);
        h.shutdown().await;
    })
    .await
}

/// a resumed session whose model's provider plugin the user disabled keeps that model
/// (no silent switch to another provider — Codex session/mod.rs:1322-1340, Kimi
/// provider-manager.ts:96-104); its prompt is "not sent" saying the provider is disabled and how
/// to go on, and is not added to the conversation (the frontend puts it back in the input box).
#[tokio::test]
async fn a_resumed_session_with_a_disabled_provider_keeps_its_model_and_refuses() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo' }\nmare.plugin('provider-anthropic', { enabled = false })\n",
        )
        .unwrap();
        env.write_session(
            "2026-01-01T00-00-00_dp.jsonl",
            &[
                msg("m1", None, "user", "earlier"),
                msg("m2", Some("m1"), "assistant", "answer"),
                json!({ "type": "model", "model": "anthropic/claude-sonnet-5-5", "at_entry_id": "m2" }),
            ],
        );
        let h = env
            .host()
            .user_config(false)
            .config(json!({ "plugins": { "provider-faux": { "script_json": json!([{ "text": "switched provider" }]).to_string() } } }))
            .resume()
            .build()
            .await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "CM", h.service_table("commands").unwrap());
        let model: String = eval(&h, "return L:current().ref").await;
        assert_eq!(model, "anthropic/claude-sonnet-5-5", "the session's model stays");
        let prompt = h.prompt("go on", json!({})).await.unwrap();
        assert_eq!(prompt["not_sent"], true, "{prompt}");
        let error = prompt["error"]["message"].as_str().unwrap();
        assert!(
            error.starts_with("model anthropic/claude-sonnet-5-5: provider anthropic is disabled")
                && error.contains("enable it")
                && error.contains("/model"),
            "{error}"
        );
        let messages: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert!(messages.contains("earlier") && !messages.contains("go on"), "{messages}");
        exec(&h, r#"CM:run("/model faux/echo")"#).await;
        let switched = h.prompt("retry", json!({})).await.unwrap();
        assert!(switched["error"].is_null(), "the user can switch to a usable provider: {switched}");
        assert_eq!(switched["text"], "switched provider");
        h.shutdown().await;
    })
    .await
}

/// The names of the methods a service's built-in contract requires, as Lua string literals.
fn required_methods(service: &str) -> String {
    let lua = mlua::Lua::new();
    let value: mlua::Value = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
    let contracts = mare_kernel::contracts::parse_contracts(&lua, value).unwrap();
    contracts
        .get(service)
        .map(|c| c.methods.iter().filter(|(_, spec)| !spec.optional).map(|(m, _)| format!("'{m}'")).collect::<Vec<_>>())
        .unwrap_or_default()
        .join(", ")
}

/// a plugin that calls `ctx:provide` for a core service (here `llm`) is that
/// service's replacement even without declaring it (reference implementation: cordis marks the provider when
/// `ctx.provide` runs) — its failure blocks requests like a declared replacement's. A runtime
/// provide of a service that isn't core still only warns.
#[tokio::test]
async fn an_undeclared_replacement_of_a_core_service_blocks_when_it_fails() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = env.config_dir().join("init.lua");
        let setup = "mare.setup { model = 'faux/echo' }";
        let dir = env.config_dir().join("plugins");
        for (name, service, blocks) in [("my-llm", "llm", true), ("my-extra", "extra", false)] {
            // one provider per service: a replacement of the built-in llm is one with the built-in
            // turned off (otherwise the provide is refused already_provided and llm keeps running)
            let builtin_off = if service == "llm" { "\nmare.plugin('llm', { enabled = false })" } else { "" };
            std::fs::write(&init, format!("{setup}{builtin_off}")).unwrap();
            // the stand-in implements exactly the contract's required methods (none for `extra`)
            let methods = required_methods(service);
            let plugin = dir.join(name).join("init.lua");
            std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
            std::fs::write(
                &plugin,
                format!(
                    "return {{ name = '{name}', apply = function(ctx)\n\
                     local S = require('cordis').class('S', mare.Service)\n\
                     function S:constructor(c) S.super.constructor(self, c, '{service}') end\n\
                     for _, m in ipairs({{ {methods} }}) do S[m] = function() end end\n\
                     ctx:plugin(S) end }}"
                ),
            )
            .unwrap();
            let opts = env.host().user_config(false).no_session().opts;
            let h = mare_host::Host::new(opts).await.expect("startup must not refuse a failed plugin");
            // Precondition: the stand-in must actually have attempted the runtime provide under its
            // own plugin identity (and failed for not declaring it); otherwise non-attribution
            // assertions below could pass vacuously with `llm` merely missing.
            assert!(
                h.kernel.provided(name).iter().any(|s| s == service),
                "{name} never recorded a runtime provide of `{service}` (provided: {:?}, problems: {:?})",
                h.kernel.provided(name),
                h.kernel.problems()
            );
            assert!(
                !h.kernel.plugin_active(name),
                "{name} must fail when its undeclared provide of `{service}` is refused"
            );
            let reason = blocked_reason(&h).await;
            if blocks {
                let reason = reason.expect("a failed replacement of llm blocks");
                assert!(reason.contains(name) && reason.contains("does not declare it"), "{reason}");
                let prompt = h.prompt("hello", json!({})).await.unwrap();
                assert_eq!(prompt["not_sent"], true, "{prompt}");
            } else {
                assert_eq!(reason, None, "not a core service: a warning only");
                assert!(h.plugin_warnings().iter().any(|w| w.contains(name)), "{:?}", h.plugin_warnings());
            }
            std::fs::remove_dir_all(plugin.parent().unwrap()).unwrap();
            std::fs::write(&init, setup).unwrap();
            h.reload_all().await.unwrap();
            assert_eq!(blocked_reason(&h).await, None, "{name}: a full reload lifts it");
            h.shutdown().await;
        }
    })
    .await
}

/// the minimal profile keeps every built-in provider (Codex: a profile only picks
/// the model, every built-in provider is always there), so a resumed session or a `/model`
/// choice in state.json whose provider isn't the configured model's sends normally — no switch,
/// no refusal. A provider the user disabled explicitly still refuses.
#[tokio::test]
async fn the_minimal_profile_keeps_every_builtin_provider() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::create_dir_all(env.home.path().join("state")).unwrap();
        let init = env.config_dir().join("init.lua");
        let minimal = "mare.setup { plugins = { preset = 'minimal' } }\n";

        // state.json's /model choice (init.lua names no model)
        std::fs::write(&init, minimal).unwrap();
        std::fs::write(env.home.path().join("state/state.json"), r#"{"version":1,"model":"faux/echo2"}"#).unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        let prompt = h.prompt("from state", json!({})).await.unwrap();
        assert!(prompt["error"].is_null(), "{prompt}");
        assert_eq!(prompt["text"], "echo: from state");
        h.shutdown().await;
        std::fs::remove_file(env.home.path().join("state/state.json")).unwrap();

        // a resumed session whose model belongs to another built-in provider than init.lua's
        let openai = "mare.setup { model = 'openai/gpt-6.1-terra' }\n";
        std::fs::write(&init, format!("{minimal}{openai}")).unwrap();
        env.write_session(
            "2026-01-01T00-00-00_mn.jsonl",
            &[
                msg("m1", None, "user", "earlier"),
                msg("m2", Some("m1"), "assistant", "answer"),
                json!({ "type": "model", "model": "faux/echo", "at_entry_id": "m2" }),
            ],
        );
        let h = env.host().user_config(false).resume().build().await;
        global(&h, "L", h.service_table("llm").unwrap());
        assert_eq!(eval::<String>(&h, "return L:current().ref").await, "faux/echo", "the session's model");
        let prompt = h.prompt("go on", json!({})).await.unwrap();
        assert!(prompt["error"].is_null(), "{prompt}");
        assert_eq!(prompt["text"], "echo: go on");
        h.shutdown().await;

        // the same session with its provider disabled explicitly: kept, and refused
        std::fs::write(&init, format!("{minimal}{openai}mare.plugin('provider-faux', {{ enabled = false }})\n"))
            .unwrap();
        let h = env.host().user_config(false).resume().build().await;
        global(&h, "L", h.service_table("llm").unwrap());
        assert_eq!(eval::<String>(&h, "return L:current().ref").await, "faux/echo");
        let prompt = h.prompt("go on", json!({})).await.unwrap();
        assert_eq!(prompt["not_sent"], true, "{prompt}");
        assert!(prompt["error"]["message"].as_str().unwrap().contains("provider faux is disabled"), "{prompt}");
        h.shutdown().await;
    })
    .await
}

const STEP_LATE: &str = r#"return { name = "step-late", inject = { "llm" }, apply = function(ctx)
  ctx.llm:readiness("step-late", { order = 25,
    prepare = function() table.insert(mare._steps, "late") end,
    ready = function() return { decision = "deny", reason = "late step refuses" } end })
end }"#;
const STEP_EARLY: &str = r#"return { name = "step-early", inject = { "llm" }, apply = function(ctx)
  ctx.llm:readiness("step-early", { order = 15,
    prepare = function() table.insert(mare._steps, "early") end,
    ready = function() return { decision = "allow" } end })
end }"#;

/// handoff cke-interface "Ordered chains": readiness steps registered with `llm:readiness` run in
/// ascending `order` whatever the registration order, the first non-allow vote refuses the prompt
/// with its reason, and a step ends with the plugin that registered it.
#[tokio::test]
async fn readiness_steps_run_in_order_and_go_with_their_plugin() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        // the later-ordered step's plugin is written (and registers) first
        std::fs::write(dir.join("step-late.lua"), STEP_LATE).unwrap();
        std::fs::write(dir.join("step-early.lua"), STEP_EARLY).unwrap();
        let h = env.host().user_plugins().faux(json!([{ "text": "ok" }])).no_session().build().await;
        exec(&h, "mare._steps = {}").await;
        let refused = h.prompt("hello", json!({})).await.unwrap();
        assert!(refused["error"]["message"].as_str().unwrap_or_default().contains("late step refuses"), "{refused}");
        let order: Vec<String> = eval(&h, "return mare._steps").await;
        assert_eq!(order, ["early", "late"], "ascending order, not registration order");

        std::fs::remove_file(dir.join("step-late.lua")).unwrap();
        h.reload_all().await.unwrap();
        exec(&h, "mare._steps = {}").await;
        let sent = h.prompt("hello", json!({})).await.unwrap();
        assert!(sent["error"].is_null(), "the refusing step went with its plugin: {sent}");
        let order: Vec<String> = eval(&h, "return mare._steps").await;
        assert_eq!(order, ["early"]);
        h.shutdown().await;
    })
    .await
}

/// The readiness list (mare_lib.ordered): equal orders run in registration order, and the removal
/// `llm:readiness` returns takes a step out at once.
#[tokio::test]
async fn equal_order_readiness_steps_keep_registration_order_and_a_removed_one_is_gone() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "ok" }])).no_session().build().await;
        global(&h, "L", h.service_table("llm").unwrap());
        exec(
            &h,
            r#"mare._steps = {}
            local function step(label)
              return { order = 500, prepare = function() table.insert(mare._steps, label) end }
            end
            L:readiness("first", step("first"))
            local dropped = L:readiness("dropped", step("dropped"))
            L:readiness("second", step("second"))
            dropped()"#,
        )
        .await;
        let sent = h.prompt("hello", json!({})).await.unwrap();
        assert!(sent["error"].is_null(), "{sent}");
        let order: Vec<String> = eval(&h, "return mare._steps").await;
        assert_eq!(order, ["first", "second"]);
        h.shutdown().await;
    })
    .await
}

/// Wall-clock rollbacks must not stall monotonic in-process timers (`session/refresh.catch_up`,
/// `llm/prepare` retry budget) or pin persisted `fetched_at` catalog caches forever.
#[tokio::test]
async fn wall_clock_rollback_does_not_stall_catch_up_prepare_or_catalog_cache() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("DEEPSEEK_API_KEY", "test-deepseek");
        let server = MockServer::start().await;
        server.route(
            "/compat/models",
            vec![Resp::status(200, r#"{"data":[{"id":"deepseek-refreshed","display_name":"Refreshed"}]}"#)],
        );

        // Pre-populate models cache with a future `fetched_at` (simulating a wall-clock rollback).
        let cache_dir = env.home.path().join("state/models-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let future_epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 7200;
        std::fs::write(
            cache_dir.join("deepseek.json"),
            format!(r#"{{"fetched_at":{future_epoch},"models":[{{"id":"deepseek-future-stale"}}]}}"#),
        )
        .unwrap();

        let h = env
            .host()
            .faux(json!([{ "text": "ok" }]))
            .config(json!({
                "providers": {
                    "deepseek": { "base_url": format!("{}/compat", server.base) }
                }
            }))
            .build()
            .await;
        global(&h, "L", h.service_table("llm").unwrap());

        // 1. session/refresh.catch_up must use monotonic clock, not mare.task.now().
        let catch_up_calls: i64 = eval(
            &h,
            r#"local orig_now = mare.task.now
               local calls = 0
               local refresh = require("session.refresh")
               local ok, err = pcall(function()
                 local r = refresh({ spawn = function(_, fn) fn() end }, function() end)
                 r.reconcile = function() calls = calls + 1 end
                 r.catch_up()
                 -- Roll wall clock backwards by 1 hour while monotonic clock advances past 2s throttle.
                 mare.task.now = function() return orig_now() - 3600000 end
                 mare.task.sleep(2100)
                 r.catch_up()
               end)
               mare.task.now = orig_now
               assert(ok, tostring(err))
               return calls"#,
        )
        .await;
        assert_eq!(catch_up_calls, 2, "catch_up must still reconcile after wall-clock rollback");

        // 2. llm/prepare BUDGET_MS must use monotonic clock, not mare.task.now().
        let prep_attempts: i64 = eval(
            &h,
            r#"local orig_now = mare.task.now
               local prep = require("llm.prepare")
               local orig_budget, orig_wait = prep.BUDGET_MS, prep.MAX_WAIT_MS
               prep.BUDGET_MS = 50
               prep.MAX_WAIT_MS = 40
               local attempts = 0
               local ok, err = pcall(function()
                 local fake_now = orig_now()
                 mare.task.now = function()
                   -- Roll wall clock backwards on every call so wall-clock elapsed stays negative.
                   fake_now = fake_now - 60000
                   return fake_now
                 end
                 local prov = {
                   prepare = function()
                     attempts = attempts + 1
                     mare.task.sleep(35)
                     error({ message = "tmp 503", kind = "server", retryable = true, retry_after_ms = 40 }, 0)
                   end,
                 }
                 prep.run(prov, { provider = "faux", id = "echo" }, {})
               end)
               mare.task.now = orig_now
               prep.BUDGET_MS, prep.MAX_WAIT_MS = orig_budget, orig_wait
               assert(ok, tostring(err))
               return attempts"#,
        )
        .await;
        assert_eq!(
            prep_attempts, 1,
            "llm/prepare must stop at BUDGET_MS (attempt 1 = 35ms + wait 40ms = 75ms > 50ms budget, not 3 attempts)"
        );

        // 3. llm/catalog must treat a future `fetched_at` (negative age) as expired and refresh.
        let has_refreshed: bool = eval(
            &h,
            r#"L:refresh_catalog({ name = "deepseek", min_age_ms = 60000 })
               local m = L:resolve("deepseek/deepseek-refreshed")
               return m ~= nil and m.listed == true"#,
        )
        .await;
        assert!(has_refreshed, "future fetched_at on disk must be treated as expired and refreshed");
        assert!(
            server.count("/compat/models") >= 1,
            "mock /models endpoint must be hit when fetched_at is in the future"
        );
        h.shutdown().await;
    })
    .await
}
