//! Catalogue refresh concurrency, locking, cancellation, and invalidation tests.

use super::*;

/// Self-authored: updating capabilities of an already-listed or static model
/// via `refresh_catalog` must emit `llm/providers-changed` even when model count does not change.
#[tokio::test]
async fn refresh_catalog_emits_providers_changed_on_capability_change() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![
                Resp::status(200, r#"{"data":[{"id":"claude-cap-probe","display_name":"Cap Probe"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-cap-probe","display_name":"Cap Probe","max_tokens":32768}]}"#),
            ],
        );
        let probe_dir = env.config_dir().join("plugins/cap-probe");
        std::fs::create_dir_all(&probe_dir).unwrap();
        std::fs::write(
            probe_dir.join("init.lua"),
            r#"return {
                name = "cap-probe",
                provide = { "cap_probe" },
                contracts = {
                    cap_probe = {
                        methods = {
                            count = {
                                doc = "count",
                                expose = "lua",
                                params = { type = "object", properties = {}, additionalProperties = false },
                                order = {},
                                result = { type = "integer" },
                            },
                        },
                    },
                },
                apply = function(ctx)
                    local count = 0
                    ctx:on("llm/providers-changed", function() count = count + 1 end)
                    local Svc = require("cordis").class("CapProbe", mare.Service)
                    function Svc:constructor(c)
                        Svc.super.constructor(self, c, "cap_probe")
                    end
                    function Svc:count()
                        return count
                    end
                    Svc.new(ctx)
                end,
            }"#,
        )
        .unwrap();
        let h = env
            .host()
            .user_plugins()
            .config(json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-anthropic": { "base_url": format!("{}/anthropic", server.base) }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        let _: bool = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, anthropic = { key = "sk-ant-cap-1" } }), { mode = 384 })
LLM:refresh_catalog({ name = "anthropic", force = true })
return true
"#,
        )
        .await;
        let first_events: i64 = h
            .call_service("cap_probe", "count", mlua::MultiValue::new())
            .await
            .unwrap()
            .into_iter()
            .next()
            .and_then(|v| v.as_integer())
            .unwrap();
        let updated_max_output: i64 = eval(
            &h,
            r#"
LLM:refresh_catalog({ name = "anthropic", force = true })
local m = assert(LLM:resolve("anthropic/claude-cap-probe"))
return m.max_output or 0
"#,
        )
        .await;
        let second_events: i64 = h
            .call_service("cap_probe", "count", mlua::MultiValue::new())
            .await
            .unwrap()
            .into_iter()
            .next()
            .and_then(|v| v.as_integer())
            .unwrap();
        assert!(
            first_events >= 1 && second_events > first_events && updated_max_output == 32768,
            "capability change on existing model must emit llm/providers-changed (first={first_events}, second={second_events}, max_output={updated_max_output})"
        );
        h.shutdown().await;
    })
    .await
}
/// Self-authored: a refresh that already passed the last `refresh_gen`/`scope` check
/// before `write_catalog` must not overwrite a newer account's in-memory or on-disk catalog after
/// yielding inside `mare.fs.read`/`write`.
#[tokio::test]
async fn refresh_models_discards_stale_result_after_last_check_async_io() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/codex/codex/models",
            vec![
                Resp::status(
                    200,
                    r#"{"models":[{"slug":"gpt-only-a","visibility":"list"}]}"#,
                ),
                Resp::status(
                    200,
                    r#"{"models":[{"slug":"gpt-only-b","visibility":"list"}]}"#,
                ),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-openai": { "codex_base_url": format!("{}/codex", server.base) }
                }
            }),
        )
        .await;
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());

        exec(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-a", refresh = "ref-a", expires = 9999999999999, account_id = "acct-a" }
}), { mode = 384 })
local orig_read = mare.fs.read
local gate_tx, gate_rx = mare.task.channel()
local entered_tx, entered_rx = mare.task.channel()
local armed = true
mare.fs.read = function(path)
  if armed and type(path) == "string" and path:find("models%-cache/openai%.json$") then
    armed = false
    entered_tx:send(true)
    gate_rx:recv()
  end
  return orig_read(path)
end
_G.__g29b_entered_rx = entered_rx
_G.__g29b_gate_tx = gate_tx
"#,
        )
        .await;

        let h_bg = h.clone();
        let bg = tokio::task::spawn_local(async move {
            exec(&h_bg, r#"LLM:refresh_models("openai", { force = true })"#).await;
        });

        exec(&h, r#"_G.__g29b_entered_rx:recv()"#).await;

        // While A is suspended inside `write_catalog`'s `mare.fs.read`, B logs in and starts refreshing.
        exec(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-b", refresh = "ref-b", expires = 9999999999999, account_id = "acct-b" }
}), { mode = 384 })
C.cordis:get("bus"):emit("llm/login", { provider = "openai" })
"#,
        )
        .await;
        let h_bg2 = h.clone();
        let bg_b = tokio::task::spawn_local(async move {
            exec(&h_bg2, r#"LLM:refresh_models("openai", { force = true })"#).await;
        });
        wait_until(10, "B's /codex/models request", || {
            let server = &server;
            async move { server.count("/codex/codex/models") >= 2 }
        })
        .await;
        exec(&h, r#"_G.__g29b_gate_tx:send(true)"#).await;
        bg.await.unwrap();
        bg_b.await.unwrap();

        let (has_b, has_a): (bool, bool) = eval(
            &h,
            r#"
local mb = LLM:resolve("openai/gpt-only-b")
local ma = LLM:resolve("openai/gpt-only-a")
return (mb ~= nil and mb.listed == true), (ma ~= nil and ma.listed == true)
"#,
        )
        .await;
        assert!(has_b && !has_a, "stale A must not overwrite B after last check (has_b={has_b}, has_a={has_a})");
        let cache = std::fs::read_to_string(env.home.path().join("state/models-cache/openai.json")).unwrap();
        assert!(cache.contains("gpt-only-b"), "disk cache must preserve B's scope: {cache}");
        h.shutdown().await;
    })
    .await
}
/// Self-authored: `mare.model` overrides may contain Lua mapping functions in
/// `params`; catalog refresh comparison must not fail with `encode_strict` function errors.
#[tokio::test]
async fn refresh_catalog_handles_builtin_model_with_function_param_override() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/codex/codex/models",
            vec![
                Resp::status(
                    200,
                    r#"{"models":[{"slug":"gpt-6.1-terra","visibility":"list","default_reasoning_level":"high"}]}"#,
                ),
                Resp::status(
                    200,
                    r#"{"models":[{"slug":"gpt-6.1-terra","visibility":"list","default_reasoning_level":"low"}]}"#,
                ),
            ],
        );
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
mare.model({
  provider = "openai",
  id = "gpt-6.1-terra",
  params = {
    effort = {
      map = function(v) return v end,
    },
  },
})
"#,
        )
        .unwrap();
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-openai": { "codex_base_url": format!("{}/codex", server.base) }
                }
            }),
        )
        .await;
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());

        let (ok1, ok2, eff): (bool, bool, String) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-1", refresh = "ref-1", expires = 9999999999999, account_id = "acct-1" }
}), { mode = 384 })
local r1 = LLM:refresh_catalog({ name = "openai", force = true })
local r2 = LLM:refresh_catalog({ name = "openai", force = true })
local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
return type(r1.openai) == "number", type(r2.openai) == "number", tostring(m.default_effort)
"#,
        )
        .await;
        assert!(ok1 && ok2 && eff == "low", "function param override must not break refresh comparison: ok1={ok1} ok2={ok2} eff={eff}");
        h.shutdown().await;
    })
    .await
}
/// Self-authored: closing/aborting a refresh coroutine while it holds the
/// per-provider catalog lock during yieldable file I/O must release the lock via `<close>`
/// so a subsequent refresh completes without hanging.
#[tokio::test]
async fn refresh_models_releases_catalog_lock_when_coroutine_is_closed() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![
                Resp::status(200, r#"{"data":[{"id":"claude-first"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-second"}]}"#),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "catalog_refresh_ms": 0 } } })).await;

        let (second_ok, has_second): (bool, bool) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, anthropic = { key = "sk-close-test" } }), { mode = 384 })
local orig_write = mare.fs.write
local entered_tx, entered_rx = mare.task.channel()
local gate_tx, gate_rx = mare.task.channel()
local armed = true
mare.fs.write = function(path, data, opts)
  if armed and type(path) == "string" and path:find("models%-cache/anthropic%.json$") then
    armed = false
    entered_tx:send(true)
    gate_rx:recv()
  end
  return orig_write(path, data, opts)
end

local first = mare.task.spawn(function()
  return LLM:refresh_models("anthropic", { force = true })
end)
assert(mare.task.timeout(2000, function() return entered_rx:recv() end), "first refresh did not enter write_catalog")
first:abort()
pcall(first.wait, first)
gate_tx:send(true)
mare.fs.write = orig_write

local ok = mare.task.timeout(2000, function()
  LLM:refresh_models("anthropic", { force = true })
  return true
end)
local m = LLM:resolve("anthropic/claude-second")
return ok == true, (m ~= nil and m.listed == true)
"#,
        )
        .await;

        assert!(second_ok, "second refresh must not hang after first coroutine is closed mid-refresh");
        assert!(has_second, "second refresh must publish claude-second");
        h.shutdown().await;
    })
    .await
}
/// Ref: reference implementation (rescopes on another
/// credential and discards the previous credential's result).
/// Self-authored: if `catalog_scope` yields during the final generation check
/// (in both `refresh_models` and `invalidate`) after reading the old credential, a concurrent
/// login/refresh must not allow the stale coroutine to re-publish old models.
#[tokio::test]
async fn stale_refresh_and_invalidate_discarded_when_scope_yields_during_final_check() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![
                Resp::status(200, r#"{"data":[{"id":"claude-old-a"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-new-b"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-inv-b"}]}"#),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "catalog_refresh_ms": 0 } } })).await;
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());

        let (a_not_republished, b_present, inv_a_not_republished, inv_b_present): (bool, bool, bool, bool) = eval(
            &h,
            r#"
local orig_read = mare.fs.read
local orig_write = mare.fs.write
orig_write(mare.paths.auth, mare.json.encode({ version = 1, anthropic = { key = "sk-cred-a" } }), { mode = 384 })

-- Part 1: refresh_models final check yields inside catalog_scope after reading sk-cred-a
local arm_read = false
local entered_tx, entered_rx = mare.task.channel()
local gate_tx, gate_rx = mare.task.channel()
mare.fs.write = function(path, data, opts)
  local r, err = orig_write(path, data, opts)
  if type(path) == "string" and path:find("models%-cache/anthropic%.json$") then
    arm_read = true
  end
  return r, err
end
mare.fs.read = function(path)
  local content, err = orig_read(path)
  if arm_read and path == mare.paths.auth then
    arm_read = false
    entered_tx:send(true)
    gate_rx:recv()
  end
  return content, err
end

local co_a = mare.task.spawn(function()
  return LLM:refresh_models("anthropic", { force = true })
end)
assert(mare.task.timeout(2000, function() return entered_rx:recv() end), "refresh A did not reach final scope read")

mare.fs.write = orig_write
mare.fs.read = orig_read
orig_write(mare.paths.auth, mare.json.encode({ version = 1, anthropic = { key = "sk-cred-b" } }), { mode = 384 })
C.cordis:get("bus"):emit("llm/login", { provider = "anthropic" })

gate_tx:send(true)
assert(mare.task.timeout(2000, function() return co_a:wait() ~= nil or true end), "refresh A did not finish")
local a_after_refresh = LLM:resolve("anthropic/claude-old-a")
LLM:refresh_models("anthropic", { force = true })
local b_after_refresh = LLM:resolve("anthropic/claude-new-b")

-- Part 2: invalidate() final check yields inside catalog_scope after reading sk-cred-a
-- First ensure sk-cred-a has a cached entry on disk? Already written in Part 1!
orig_write(mare.paths.auth, mare.json.encode({ version = 1, anthropic = { key = "sk-cred-a" } }), { mode = 384 })
local arm_inv_cache = true
local arm_inv_auth = false
local inv_entered_tx, inv_entered_rx = mare.task.channel()
local inv_gate_tx, inv_gate_rx = mare.task.channel()
mare.fs.read = function(path)
  local content, err = orig_read(path)
  if arm_inv_cache and type(path) == "string" and path:find("models%-cache/anthropic%.json$") then
    arm_inv_cache = false
    arm_inv_auth = true
  elseif arm_inv_auth and path == mare.paths.auth then
    arm_inv_auth = false
    inv_entered_tx:send(true)
    inv_gate_rx:recv()
  end
  return content, err
end

local co_inv = mare.task.spawn(function()
  C.cordis:get("bus"):emit("llm/login", { provider = "anthropic" })
  return true
end)
assert(mare.task.timeout(2000, function() return inv_entered_rx:recv() end), "invalidate A did not reach final scope read")

mare.fs.read = orig_read
orig_write(mare.paths.auth, mare.json.encode({ version = 1, anthropic = { key = "sk-cred-b2" } }), { mode = 384 })
C.cordis:get("bus"):emit("llm/login", { provider = "anthropic" })
LLM:refresh_models("anthropic", { force = true })

inv_gate_tx:send(true)
assert(mare.task.timeout(2000, function() return co_inv:wait() end), "invalidate A did not finish")
local a_after_inv = LLM:resolve("anthropic/claude-old-a")
local b_after_inv = LLM:resolve("anthropic/claude-inv-b")

return (a_after_refresh == nil or a_after_refresh.listed ~= true),
  (b_after_refresh ~= nil and b_after_refresh.listed == true),
  (a_after_inv == nil or a_after_inv.listed ~= true),
  (b_after_inv ~= nil and b_after_inv.listed == true)
"#,
        )
        .await;

        assert!(a_not_republished, "stale refresh A must not re-publish claude-old-a after cred B login");
        assert!(b_present, "cred B claude-new-b must remain published");
        assert!(inv_a_not_republished, "stale invalidate A must not re-publish claude-old-a after cred B2 login");
        assert!(inv_b_present, "cred B2 claude-inv-b must remain published");
        h.shutdown().await;
    })
    .await
}
/// Ref: reference implementation/Codex/Kimi Code 三家均无同类测试.
/// Self-authored: aborting a refresh coroutine while a background disk write is
/// already in flight must not release the catalog lock before the in-flight write finishes, so a
/// subsequent refresh cannot be overwritten on disk by the late write.
#[tokio::test]
async fn cancelled_in_flight_write_does_not_overwrite_newer_catalog_on_disk() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![
                Resp::status(200, r#"{"data":[{"id":"claude-late-first"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-newer-second"}]}"#),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "catalog_refresh_ms": 0 } } })).await;

        let (second_blocked_before_first_finishes, second_ok, has_second, disk_has_second, disk_no_first): (
            bool,
            bool,
            bool,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, anthropic = { key = "sk-late-write" } }), { mode = 384 })
local orig_write = mare.fs.write
local orig_http = mare.http.request
local orig_spawn = mare.task.spawn
local first_entered_tx, first_entered_rx = mare.task.channel()
local first_gate_tx, first_gate_rx = mare.task.channel()
local first_finished_tx, first_finished_rx = mare.task.channel()
local second_http_done_tx, second_http_done_rx = mare.task.channel()
local second_lock_entered_tx, second_lock_entered_rx = mare.task.channel()
local second_http_done = false
local lock_hooked = false
local second_write_entered = false
local write_seq = 0
local http_seq = 0

mare.http.request = function(req)
  local resp = orig_http(req)
  if type(req) == "table" and type(req.url) == "string" and req.url:find("/anthropic/v1/models", 1, true) then
    http_seq = http_seq + 1
    if http_seq == 2 then
      second_http_done = true
      second_http_done_tx:send(true)
    end
  end
  return resp
end

mare.task.spawn = function(cb)
  if second_http_done and not lock_hooked then
    lock_hooked = true
    return orig_spawn(function(...)
      second_lock_entered_tx:send(true)
      return cb(...)
    end)
  end
  return orig_spawn(cb)
end

mare.fs.write = function(path, data, opts)
  if type(path) == "string" and path:find("models%-cache/anthropic%.json$") then
    write_seq = write_seq + 1
    if write_seq == 1 then
      local write_done_tx, write_done_rx = mare.task.channel()
      orig_spawn(function()
        first_entered_tx:send(true)
        first_gate_rx:recv()
        local r, err = orig_write(path, data, opts)
        first_finished_tx:send(true)
        pcall(write_done_tx.send, write_done_tx, { r = r, err = err })
      end)
      local res = write_done_rx:recv()
      return res.r, res.err
    elseif write_seq == 2 then
      second_write_entered = true
    end
  end
  return orig_write(path, data, opts)
end

local first = orig_spawn(function()
  return LLM:refresh_models("anthropic", { force = true })
end)
assert(mare.task.timeout(2000, function() return first_entered_rx:recv() end), "first refresh did not enter write")
first:abort()
pcall(first.wait, first)

-- Keep first blocked in its in-flight write while second starts, finishes HTTP, and waits
-- inside with_catalog_lock's spawned lock task.
local second = orig_spawn(function()
  return LLM:refresh_models("anthropic", { force = true })
end)
assert(mare.task.timeout(2000, function() return second_http_done_rx:recv() end), "second refresh did not finish HTTP")
assert(mare.task.timeout(2000, function() return second_lock_entered_rx:recv() end), "second refresh did not enter catalog lock task")
for _ = 1, 8 do
  local ytx, yrx = mare.task.channel()
  orig_spawn(function() ytx:send(true) end)
  yrx:recv()
end
local blocked = not second_write_entered

-- Only now release first's in-flight write, then wait for both to complete.
first_gate_tx:send(true)
assert(mare.task.timeout(2000, function() return first_finished_rx:recv() end), "first in-flight write did not finish")
local ok = mare.task.timeout(2000, function()
  second:wait()
  return true
end)
mare.task.spawn = orig_spawn
mare.fs.write = orig_write
mare.http.request = orig_http

local disk = mare.fs.read(mare.paths.state .. "/models-cache/anthropic.json") or ""
local m = LLM:resolve("anthropic/claude-newer-second")
return blocked,
  ok == true,
  (m ~= nil and m.listed == true),
  disk:find("claude-newer-second", 1, true) ~= nil,
  disk:find("claude-late-first", 1, true) == nil
"#,
        )
        .await;

        assert!(
            second_blocked_before_first_finishes,
            "second refresh must not enter protected write before cancelled first write finishes"
        );
        assert!(second_ok, "second refresh must complete");
        assert!(has_second, "second refresh must publish claude-newer-second");
        assert!(disk_has_second, "disk cache must contain claude-newer-second");
        assert!(disk_no_first, "late cancelled write must not overwrite newer disk cache with claude-late-first");
        h.shutdown().await;
    })
    .await
}
