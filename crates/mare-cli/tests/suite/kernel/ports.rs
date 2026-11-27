//! Semantics ported from Cordis (upstream specs via cordis-lua, which keeps their file:line),
//! reference implementation vendored Cordis hardening (reference implementation V6, V25;
//! packages/extensions/tool-cordis/tests/cordis-lifecycle.spec.ts) and the fidelity review
//! . Public paths only, as in semantics.rs.

use super::{host, write_plugin};
use crate::common::*;
use std::sync::Arc;

fn problems(h: &mare_host::Host) -> String {
    h.kernel.problems().join("\n")
}

/// Lua defining `Echo`, a mare.Service class publishing `echo` with a `who` contract; the
/// fragments of `$cleanup` are the Lua run when the service's fiber is disposed (not the outer
/// plugin's: `ctx:plugin(Echo)` runs the service in a child fiber, which the dependents follow);
/// `who` itself is written after it (`function Echo:who() ... end`).
macro_rules! echo_class {
    ($($cleanup:expr),+) => {
        concat!(
            r#"local Echo = require("cordis").class("Echo", mare.Service)
  function Echo:constructor(c)
    Echo.super.constructor(self, c, "echo", { contract = { methods = { who = { doc = "who", expose = "lua",
      params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "string" } } } } })
    c:effect(function() return function() "#,
            $($cleanup),+,
            r#" end end)
  end
  "#
        )
    };
}

macro_rules! echo_provider {
    ($who:literal) => {
        concat!(
            r#"return { name = "echo", provide = { "echo" }, apply = function(ctx)
  mare._seq = mare._seq or {}
  "#,
            echo_class!(r#"table.insert(mare._seq, "provider")"#),
            r#"function Echo:who() return ""#,
            $who,
            r#"" end
  ctx:plugin(Echo)
end }"#
        )
    };
}

const DEPENDENT: &str = r#"return { name = "dependent", inject = { "echo" }, apply = function(ctx)
  mare._seq = mare._seq or {}
  mare._applied = (mare._applied or 0) + 1
  mare._saw = ctx.echo:who()
  ctx:effect(function() return function() table.insert(mare._seq, "dependent") end end)
end }"#;

/// The provider is replaced (its file changes): the dependent is disposed before the provider,
/// then started again on the new provider — never left on the old one. Cordis
/// packages/core/tests/service.spec.ts:190, :202 (cordis-lua test/core/service_spec.lua:186, :199).
// guarantee: dependents-stop-before-their-provider-and-restart-on-the-new-one
#[tokio::test]
async fn a_replaced_provider_disposes_its_dependents_first_and_restarts_them_on_the_new_one() {
    local(async {
        let env = TestEnv::new().await;
        let provider = write_plugin(&env, "echo", echo_provider!("one"));
        write_plugin(&env, "dependent", DEPENDENT);
        let h = host(&env).await;
        let before: String = eval(&h, "return mare._saw").await;
        assert_eq!(before, "one", "{}", problems(&h));
        std::fs::write(&provider, echo_provider!("two")).unwrap();
        h.reload_all().await.unwrap();
        let (seq, saw, applied): (Vec<String>, String, i64) =
            eval(&h, "return mare._seq, mare._saw, mare._applied").await;
        h.shutdown().await;
        assert_eq!(seq.first().map(String::as_str), Some("dependent"), "dependents go first: {seq:?}");
        assert!(seq.contains(&"provider".to_string()), "{seq:?}");
        assert_eq!((saw.as_str(), applied), ("two", 2), "the dependent restarts on the new provider");
    })
    .await
}

/// The provider goes away: the dependent is disposed (its cleanup runs), it is not a failure, and
/// it starts again when a provider comes back. Cordis service.spec.ts:7 "pending inject"
/// (cordis-lua service_spec.lua:11); reference implementation "drain effects attached while pending".
// guarantee: dependent-waits-while-its-provider-is-gone
#[tokio::test]
async fn a_dependent_waits_when_its_provider_goes_and_returns_with_it() {
    local(async {
        let env = TestEnv::new().await;
        let provider = write_plugin(&env, "echo", echo_provider!("one"));
        write_plugin(&env, "dependent", DEPENDENT);
        let h = host(&env).await;
        std::fs::remove_file(&provider).unwrap();
        h.reload_all().await.unwrap();
        let seq: Vec<String> = eval(&h, "return mare._seq").await;
        assert!(seq.contains(&"dependent".to_string()), "the dependent's cleanup ran: {seq:?}");
        assert!(!problems(&h).contains("failed"), "waiting is not a failure: {}", problems(&h));
        std::fs::write(&provider, echo_provider!("back")).unwrap();
        h.reload_all().await.unwrap();
        let saw: String = eval(&h, "return mare._saw").await;
        h.shutdown().await;
        assert_eq!(saw, "back", "it starts again with the returned provider");
    })
    .await
}

const LATE_REGISTRAR: &str = r#"return { name = "late-reg", apply = function(ctx)
  ctx:effect(function() return function()
    mare._cleanup_entered = true
    local ok = pcall(ctx.on, ctx, "probe/late", function() mare._late_ran = true end)
    mare._late_reg_ok = ok
  end end)
end }"#;

/// A registration made from a cleanup, while the plugin unloads, must not escape the unload: the
/// listener never runs afterwards. reference implementation V6 ("Effect creation is rejected while
/// the owner is UNLOADING"), packages/extensions/tool-cordis/tests/cordis-lifecycle.spec.ts:98;
/// cordis-lua docs/deviations.md F2.
// guarantee: no-registration-escapes-an-unload
#[tokio::test]
async fn a_registration_made_during_unload_does_not_outlive_the_plugin() {
    local(async {
        let env = TestEnv::new().await;
        install_probe_events_plugin(&env, "probe", &[("late", "emit")]);
        let f = write_plugin(&env, "late-reg", LATE_REGISTRAR);
        let h = host(&env).await;
        std::fs::remove_file(&f).unwrap();
        h.reload_all().await.unwrap();
        exec(&h, r#"C.cordis:get("bus"):emit("probe/late")"#).await;
        let (entered, reg_ok, ran): (Option<bool>, Option<bool>, bool) =
            eval(&h, "return mare._cleanup_entered, mare._late_reg_ok, mare._late_ran == true").await;
        h.shutdown().await;
        assert_eq!(entered, Some(true), "the cleanup must have run (else nothing is proven)");
        assert_eq!(reg_ok, Some(false), "the registration during unload must be refused");
        assert!(!ran, "a listener registered during unload survived it");
    })
    .await
}

const SLOW_CLEANUP: &str = r#"return { name = "slow-cleanup", apply = function(ctx)
  ctx:effect(function() return function() mare.task.sleep(50); mare._cleaned = true end end)
end }"#;

/// Turning a plugin off waits for its (asynchronous) cleanup before the reload reports done.
/// reference implementation V25 ("disabled-entry updates await disposal").
// guarantee: turning-a-plugin-off-waits-for-its-cleanup
#[tokio::test]
async fn turning_a_plugin_off_waits_for_its_cleanup() {
    local(async {
        let env = TestEnv::new().await;
        let f = write_plugin(&env, "slow-cleanup", SLOW_CLEANUP);
        let h = host(&env).await;
        std::fs::remove_file(&f).unwrap();
        h.reload_all().await.unwrap();
        let cleaned: bool = eval(&h, "return mare._cleaned == true").await;
        h.shutdown().await;
        assert!(cleaned, "the reload returned before the plugin's cleanup finished");
    })
    .await
}

const ZERO_INTERVAL: &str = r#"return { name = "zero-every", inject = { "task" }, apply = function(ctx)
  mare._ticks = 0
  ctx:every(0, function() mare._ticks = mare._ticks + 1 end)
end }"#;

/// A zero (or already elapsed) interval still yields between runs: the host stays responsive and
/// the callback runs a bounded number of times per turn. Fidelity review
/// .
#[tokio::test]
async fn a_zero_interval_yields_between_runs() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "zero-every", ZERO_INTERVAL);
        let h = host(&env).await;
        let ticks = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            exec(&h, "mare.task.sleep(20)").await;
            eval::<i64>(&h, "return mare._ticks").await
        })
        .await
        .expect("a zero interval starved the host");
        h.shutdown().await;
        assert!(ticks > 0, "the interval never ran");
        assert!(ticks < 100_000, "the interval ran without yielding: {ticks}");
    })
    .await
}

const SHARED_A: &str = r#"return { name = "shared-a", apply = function(ctx)
  mare._shared_runs = mare._shared_runs or 0
  mare._shared = mare._shared or function() mare._shared_runs = mare._shared_runs + 1 end
  ctx:effect(function() return mare._shared end)
end }"#;
const SHARED_B: &str = r#"return { name = "shared-b", apply = function(ctx)
  mare._shared_runs = mare._shared_runs or 0
  mare._shared = mare._shared or function() mare._shared_runs = mare._shared_runs + 1 end
  ctx:effect(function() return mare._shared end)
end }"#;

/// One cleanup function registered by two plugins is two registrations: disposing one runs it
/// once and leaves the other's.
// guarantee: shared-cleanup-runs-once-per-registration
#[tokio::test]
async fn a_cleanup_shared_by_two_plugins_runs_once_per_registration() {
    local(async {
        let env = TestEnv::new().await;
        let a = write_plugin(&env, "shared-a", SHARED_A);
        let b = write_plugin(&env, "shared-b", SHARED_B);
        let h = host(&env).await;
        std::fs::remove_file(&a).unwrap();
        h.reload_all().await.unwrap();
        let after_a: i64 = eval(&h, "return mare._shared_runs").await;
        std::fs::remove_file(&b).unwrap();
        h.reload_all().await.unwrap();
        let after_b: i64 = eval(&h, "return mare._shared_runs").await;
        h.shutdown().await;
        // (today reload_all also restarts the unchanged plugin, so `after_a` counts its restart
        // too; what matters is that B's registration survives A's going and runs once more)
        assert!(after_a >= 1, "A's registration ran");
        assert_eq!(
            after_b - after_a,
            1,
            "B's registration of the shared cleanup was lost with A's: {after_a} -> {after_b}"
        );
    })
    .await
}

const HOST_PROBE: &str = r#"return { name = "host-probe", inject = { "tools" }, apply = function(ctx)
  local function leaks(x)
    if type(x) ~= "table" and type(x) ~= "userdata" then return nil end
    -- `host` itself is cordis-lua's X1 facade (latch, sleep, now, capture, spawn in the caller's
    -- own context): public and without rights of its own. The host's root is what must not leak.
    for _, k in ipairs({ "_host", "root_spawn" }) do
      local ok, v = pcall(function() return x[k] end)
      if ok and v ~= nil then return k end
    end
    return nil
  end
  mare._apply_leak = leaks(ctx) or (pcall(function() return ctx.fiber end) and leaks(ctx.fiber)) or leaks(ctx.host) or "none"
  mare._probe_ctx = ctx
  ctx.tools:register("host_probe", { description = "probe", parameters = { type = "object" },
    execute = function(_, tctx)
      mare._tool_leak = leaks(ctx) or leaks(tctx) or "none"
      return "ok"
    end })
end }"#;

/// The host's root spawn is not reachable from plugin code — neither from a plugin's ctx nor
/// from inside a tool it registered (a restricted execution must not escape through it). `ctx.host`
/// is the X1 facade, public: a task it spawns from a sandboxed execution stays in that sandbox.
/// Fidelity review ("root spawn 不可经 ctx.host /
/// Future._host 取得 … 补受限 peer、工具、host 三类端到端测试"); the restricted-peer case needs
/// an RPC peer and is left to connection fibers.
#[tokio::test]
async fn the_hosts_root_spawn_is_not_reachable_from_plugin_or_tool_code() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "host-probe", HOST_PROBE);
        let h = host(&env).await;
        global(
            &h,
            "CALL",
            mare_shared::lua::json_to_lua(
                h.lua(),
                &serde_json::json!({ "id": "t1", "name": "host_probe", "arguments": {} }),
            )
            .unwrap(),
        );
        exec(&h, "T:execute(CALL, {})").await;
        let (apply_leak, tool_leak): (String, String) =
            eval(&h, "return mare._apply_leak, tostring(mare._tool_leak)").await;
        // the facade's spawn from a sandboxed execution: the task keeps that sandbox
        let (outside, inside): (bool, Option<bool>) = eval(
            &h,
            r#"local sid = C.cordis:get("session"):info().session_id
               local item = mare.spill.save(sid, "secret")
               local outside = pcall(mare.spill.read, item.spill_id)
               local restricted = { sandbox = { level = "read-only", network = true,
                 deny_read = { mare.paths.state .. "/spill-results" }, accept_partial = { "fs_deny_read" } } }
               assert(mare.exec.run(restricted, function()
                 mare._probe_ctx.host.spawn(function()
                   mare._spawned_read = pcall(mare.spill.read, item.spill_id)
                 end)
                 return true
               end))
               for _ = 1, 100 do
                 if mare._spawned_read ~= nil then break end
                 mare.task.sleep(10)
               end
               return outside, mare._spawned_read"#,
        )
        .await;
        let p = problems(&h);
        h.shutdown().await;
        assert_eq!(apply_leak, "none", "a plugin's ctx exposes the host's root: {p}");
        assert_eq!(tool_leak, "none", "a tool's context exposes the host's root: {p}");
        assert!(outside, "precondition: the read is allowed outside the sandbox");
        assert_eq!(inside, Some(false), "a task ctx.host spawned from a sandbox ran without it");
    })
    .await
}

const BAIL_A: &str = r#"return { name = "bail-a", apply = function(ctx)
  ctx:on("probe/bail", function() mare._bail_calls = (mare._bail_calls or "") .. "a"; return nil end)
end }"#;
const BAIL_B: &str = r#"return { name = "bail-b", apply = function(ctx)
  ctx:on("probe/bail", function() mare._bail_calls = (mare._bail_calls or "") .. "b"; return "B" end)
end }"#;
const BAIL_C: &str = r#"return { name = "bail-c", apply = function(ctx)
  ctx:on("probe/bail", function() mare._bail_calls = (mare._bail_calls or "") .. "c"; return "C" end)
end }"#;

/// `bail` asks listeners in registration order and stops at the first non-nil answer, which it
/// returns. Cordis packages/core/tests/events.spec.ts (bail) via cordis-lua
/// test/core/events_spec.lua.
#[tokio::test]
async fn bail_returns_the_first_answer_in_registration_order_and_stops_there() {
    local(async {
        let env = TestEnv::new().await;
        install_probe_events_plugin(&env, "probe", &[("bail", "bail")]);
        write_plugin(&env, "bail-a", BAIL_A);
        write_plugin(&env, "bail-b", BAIL_B);
        write_plugin(&env, "bail-c", BAIL_C);
        let h = host(&env).await;
        let (answer, calls): (String, String) =
            eval(&h, r#"local r = C.cordis:bail("probe/bail") return tostring(r), mare._bail_calls"#).await;
        h.shutdown().await;
        assert_eq!((answer.as_str(), calls.as_str()), ("B", "ab"), "first non-nil answer, later listeners not asked");
    })
    .await
}

const CFG: &str = r#"return { name = "cfg", config_schema = { greeting = { type = "string", default = "default", desc = "a greeting" } },
  apply = function(ctx, cfg)
    mare._cfg_applied = (mare._cfg_applied or 0) + 1
    mare._greeting = cfg.greeting
  end }"#;
const OTHER: &str =
    r#"return { name = "other", apply = function() mare._other_applied = (mare._other_applied or 0) + 1 end }"#;

/// A host reading init.lua (and the user plugins), with `init` written first.
async fn configured(env: &TestEnv, init: &str) -> Arc<mare_host::Host> {
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(env.config_dir().join("init.lua"), init).unwrap();
    let b = env.host().user_config(false).no_session();
    let h = mare_host::Host::new(b.opts).await.unwrap();
    expose(&h);
    h
}

fn set_init(env: &TestEnv, init: &str) {
    std::fs::write(env.config_dir().join("init.lua"), init).unwrap();
}

/// An edit that does not evaluate keeps the running configuration (and its plugins); the next
/// valid edit applies. reference implementation ("keeps the last
/// good tree instead of throwing, then applies the next valid edit").
#[tokio::test]
async fn a_broken_config_edit_keeps_the_last_good_one_and_the_next_valid_edit_applies() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "cfg", CFG);
        let h = configured(&env, r#"mare.plugin("cfg", { greeting = "one" })"#).await;
        assert_eq!(eval::<String>(&h, "return mare._greeting").await, "one", "{}", problems(&h));
        set_init(&env, r#"mare.plugin("cfg", { greeting = "two" "#);
        let broken = h.reload_config().await;
        let (still, active): (String, bool) =
            eval(&h, "return mare._greeting, mare.plugins.status('cfg').active").await;
        set_init(&env, r#"mare.plugin("cfg", { greeting = "three" })"#);
        h.reload_config().await.unwrap();
        let next: String = eval(&h, "return mare._greeting").await;
        h.shutdown().await;
        assert!(broken.is_err(), "a broken init.lua must be reported");
        assert_eq!((still.as_str(), active), ("one", true), "the running plugin keeps the last good config");
        assert_eq!(next, "three", "the next valid edit applies");
    })
    .await
}

/// Changing one plugin's config restarts that plugin only (`entry:update({ config })`;
/// see cordis-lua test/core/update_spec.lua / Cordis update.spec.ts).
// guarantee: config-change-restarts-only-its-plugin
#[tokio::test]
async fn a_config_change_restarts_only_the_plugin_it_belongs_to() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "cfg", CFG);
        write_plugin(&env, "other", OTHER);
        let h = configured(&env, r#"mare.plugin("cfg", { greeting = "one" })"#).await;
        set_init(&env, r#"mare.plugin("cfg", { greeting = "two" })"#);
        h.reload_config().await.unwrap();
        let (greeting, cfg_n, other_n): (String, i64, i64) =
            eval(&h, "return mare._greeting, mare._cfg_applied, mare._other_applied").await;
        h.shutdown().await;
        assert_eq!((greeting.as_str(), cfg_n), ("two", 2), "the changed plugin restarts with its new config");
        assert_eq!(other_n, 1, "an unrelated plugin was restarted by another plugin's config change");
    })
    .await
}

/// A plugin turned off after it ran (`plugins.cfg.enabled = false`) is reported off and not running
/// by the status query (`mare.plugins.status`). reference implementation.
#[tokio::test]
async fn a_plugin_turned_off_after_running_is_reported_inactive() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "cfg", CFG);
        let h = configured(&env, r#"mare.plugin("cfg", { greeting = "one" })"#).await;
        set_init(&env, r#"mare.plugin("cfg", { enabled = false })"#);
        h.reload_config().await.unwrap();
        let (active, disabled): (bool, bool) =
            eval(&h, "local s = mare.plugins.status('cfg') return s.active, s.disabled").await;
        h.shutdown().await;
        assert_eq!((active, disabled), (false, true), "the status query must report it off and not running");
    })
    .await
}

// T3b: provider churn, inject-shaped (the old ctx:with_service cases of kernel/services.rs:
// with_service_serializes_rapid_replacements, with_service_disposal_joins_in_flight_cleanup,
// with_service_failed_callback_cleans_before_next_provider, removed with watch.rs in D3).
// The dependent logs its lifetime in mare._ev; its cleanup is asynchronous, can be held open with
// mare._release = false, and calls the provider it was started on.

macro_rules! ev_provider {
    ($who:literal) => {
        concat!(
            r#"return { name = "echo", provide = { "echo" }, apply = function(ctx)
  mare._ev = mare._ev or {}
  "#,
            echo_class!(r#"table.insert(mare._ev, "provider gone "#, $who, r#"")"#),
            r#"function Echo:who() return ""#,
            $who,
            r#"" end
  ctx:plugin(Echo)
end }"#
        )
    };
}

const CHURN_DEPENDENT: &str = r#"return { name = "dependent", inject = { "echo" }, apply = function(ctx)
  mare._ev = mare._ev or {}
  local who = ctx.echo:who()
  table.insert(mare._ev, "start " .. who)
  ctx:effect(function() return function()
    table.insert(mare._ev, "cleanup begin " .. who)
    while mare._release == false do mare.task.sleep(5) end
    mare.task.sleep(20)
    local ok, now = pcall(function() return ctx.echo:who() end)
    table.insert(mare._ev, "cleanup end " .. who .. " sees " .. (ok and tostring(now) or "error"))
  end end)
  if who == mare._fail_on then error("dependent refuses " .. who) end
end }"#;

async fn events(h: &mare_host::Host) -> Vec<String> {
    eval(h, "return mare._ev or {}").await
}

/// One dependent instance at a time: no `start` before the previous instance's cleanup ended.
fn assert_no_overlap(ev: &[String]) {
    let mut alive: Option<&str> = None;
    for e in ev {
        if let Some(who) = e.strip_prefix("start ") {
            assert!(alive.is_none(), "started on {who} while the instance on {alive:?} was not cleaned up: {ev:?}");
            alive = Some(who);
        } else if e.starts_with("cleanup end ") {
            alive = None;
        }
    }
}

const SWAPPING_PROVIDER: &str = r#"return { name = "echo", inject = { "task" }, provide = { "echo" }, apply = function(ctx)
  mare._ev = mare._ev or {}
  local contract = { contract = { methods = { who = { doc = "who", expose = "lua",
    params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "string" } } } } }
  local function serve(who)
    local Echo = require("cordis").class("Echo", mare.Service)
    function Echo:constructor(c) Echo.super.constructor(self, c, "echo", contract) end
    function Echo:who() return who end
    return ctx:plugin(Echo)
  end
  local fiber = serve("one")
  -- a swap asked while the dependent's cleanup is held must not block the asker: one worker
  -- replaces the service with the latest request
  local latest, running
  ctx:on("probe/swap", function(who)
    latest = who
    if running then return end
    running = true
    ctx:spawn(function()
      while latest do
        local next_who = latest
        latest = nil
        require("cordis").await(fiber:dispose())
        fiber = serve(next_who)
      end
      running = false
    end)
  end)
end }"#;

/// The provider replaces its service again while the dependent's cleanup for the previous
/// replacement is still running: the restarts are serialized — never two dependent instances
/// alive — and it ends on the latest service. Cordis service.spec.ts:190 (cordis-lua
/// service_spec.lua:186); old mare kernel/services.rs with_service_serializes_rapid_replacements.
#[tokio::test]
async fn rapid_service_replacements_never_overlap_two_dependent_instances() {
    local(async {
        let env = TestEnv::new().await;
        install_probe_events_plugin(&env, "probe", &[("swap", "emit")]);
        write_plugin(&env, "echo", SWAPPING_PROVIDER);
        write_plugin(&env, "dependent", CHURN_DEPENDENT);
        let h = host(&env).await;
        exec(&h, "mare._release = false").await;
        exec(&h, r#"C.cordis:get("bus"):emit("probe/swap", "two")"#).await;
        wait_until(10, "the dependent's cleanup to begin", || {
            let h = h.clone();
            async move { events(&h).await.iter().any(|e| e == "cleanup begin one") }
        })
        .await;
        exec(&h, r#"C.cordis:get("bus"):emit("probe/swap", "three")"#).await;
        // sleep-ok: negative window: the swap must not start while the cleanup holds it; `held` is read after it
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let held = events(&h).await;
        exec(&h, "mare._release = true").await;
        let mut ev = vec![];
        for _ in 0..200 {
            ev = events(&h).await;
            if ev.iter().rev().find(|e| e.starts_with("start ")).is_some_and(|e| e == "start three") {
                break;
            }
            // sleep-ok: poll interval for the swap to start (bounded, 200 tries)
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        h.shutdown().await;
        assert_eq!(held, ["start one", "cleanup begin one"], "nothing started while the cleanup was held: {ev:?}");
        assert_no_overlap(&ev);
        let last = ev.iter().rev().find(|e| e.starts_with("start ")).cloned();
        assert_eq!(last.as_deref(), Some("start three"), "the dependent ends on the latest service: {ev:?}");
    })
    .await
}

/// The provider goes while the dependent's cleanup is in flight: the provider's own disposal
/// waits for that cleanup, which can still call the provider it was started on. reference implementation
/// V25 (disposal awaits in-flight cleanup); old mare kernel/services.rs
/// with_service_disposal_joins_in_flight_cleanup (mode 0: "cleanup end A" before "provider disposed").
// guarantee: provider-outlives-dependents-in-flight-cleanup
#[tokio::test]
async fn a_provider_outlives_its_dependents_in_flight_cleanup() {
    local(async {
        let env = TestEnv::new().await;
        let provider = write_plugin(&env, "echo", ev_provider!("one"));
        write_plugin(&env, "dependent", CHURN_DEPENDENT);
        let h = host(&env).await;
        std::fs::remove_file(&provider).unwrap();
        h.reload_all().await.unwrap();
        let ev = events(&h).await;
        h.shutdown().await;
        let at = |s: &str| ev.iter().position(|e| e == s).unwrap_or_else(|| panic!("no {s:?} in {ev:?}"));
        assert!(
            at("cleanup end one sees one") < at("provider gone one"),
            "the provider went before its dependent's cleanup finished (or the cleanup lost it): {ev:?}"
        );
    })
    .await
}

/// The dependent fails to start on a replacement provider after registering a cleanup: that
/// cleanup runs to the end before anything else, and the next provider starts the dependent
/// again — a failed restart does not leave it off for good. Old mare kernel/services.rs
/// with_service_failed_callback_cleans_before_next_provider; the lifecycle design (rollback before report).
#[tokio::test]
async fn a_failed_restart_is_cleaned_up_and_the_next_provider_starts_the_dependent() {
    local(async {
        let env = TestEnv::new().await;
        let provider = write_plugin(&env, "echo", ev_provider!("one"));
        write_plugin(&env, "dependent", CHURN_DEPENDENT);
        let h = host(&env).await;
        exec(&h, "mare._fail_on = 'two'").await;
        std::fs::write(&provider, ev_provider!("two")).unwrap();
        let _ = h.reload_all().await;
        std::fs::write(&provider, ev_provider!("three")).unwrap();
        let _ = h.reload_all().await;
        let ev = events(&h).await;
        let running: bool = eval(&h, "return mare._ev[#mare._ev] == 'start three'").await;
        h.shutdown().await;
        assert_no_overlap(&ev);
        let at = |s: &str| ev.iter().position(|e| e == s).unwrap_or_else(|| panic!("no {s:?} in {ev:?}"));
        assert!(at("start two") < at("cleanup begin two"), "the failed start's cleanup ran: {ev:?}");
        assert!(at("cleanup end two sees two") < at("start three"), "{ev:?}");
        assert!(running, "the dependent runs on the next provider: {ev:?}");
    })
    .await
}
