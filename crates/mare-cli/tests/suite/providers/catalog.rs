//! Provider directory, wire `provider` factories, load-order independence, config
//! overlays, attribution, and sync/no-disk-read startup registration.

use super::*;

fn write_user_plugin(env: &TestEnv, name: &str, code: &str) {
    let dir = env.config_dir().join("plugins").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("init.lua"), code).unwrap();
}

/// Shared fixture wire plugin registering wire `fixture-chat` with a `provider(name, entry)`
/// factory, `provider_fields`, and a factory-returned `params` rule.
const FIXTURE_WIRE_PLUGIN: &str = r#"
return {
  name = "wire-fixture",
  inject = { "llm" },
  wires = { "fixture-chat" },
  apply = function(ctx)
    local llm = ctx.llm
    llm:register_wire({
      name = "fixture-chat",
      route = "chat",
      path = "/chat/completions",
      auth = { "bearer" },
      provider_fields = {
        custom_tag = "string",
      },
      encode = function(req, ep)
        return {
          method = "POST",
          url = ep.url,
          headers = { accept = "text/event-stream", ["x-custom-tag"] = ep.custom_tag },
          body = { model = req.model.id, messages = { { role = "user", content = "hi" } } },
          model = req.model.id,
        }
      end,
      decoder = function(sink)
        local text = ""
        return {
          event = function(ev)
            if ev.data and ev.data ~= "[DONE]" then
              local ok, j = pcall(mare.json.decode, ev.data)
              if ok and j and j.text then
                text = text .. j.text
                sink({ type = "text_delta", delta = j.text })
              end
            end
          end,
          finish = function()
            return { role = "assistant", content = { { type = "text", text = text } }, stop_reason = "stop", usage = {} }
          end,
        }
      end,
      provider = function(name, entry)
        local stream = llm:wire("fixture-chat")
        local function ep(_req)
          return {
            provider = name,
            label = entry.label or name,
            base_url = entry.base_url,
            url = entry.base_url .. "/chat/completions",
            auth = "bearer",
            key = "fixture-key",
            custom_tag = entry.custom_tag,
          }
        end
        return {
          route = "chat",
          wire = "fixture-chat",
          endpoint = ep,
          stream = function(req, sink)
            return stream(ep, req, sink)
          end,
          no_key = true,
          is_configured = function()
            return true
          end,
          params = entry.custom_tag and {
            {
              provider = name,
              route = "chat",
              define = {
                fixture_mode = {
                  type = "enum",
                  default = entry.custom_tag,
                  options = { { value = entry.custom_tag, wire = { chat = { fixture_mode = "$value" } } } },
                },
              },
            },
          } or nil,
        }
      end,
    })
  end,
}
"#;

/// Three vendor sources (multi-vendor pack plugin, single-vendor plugin, and user
/// `mare.provider(...)` config entry) all flow through the exact same data-entry directory and
/// wire `provider` factory without writing `stream`/`endpoint`.
#[tokio::test]
async fn three_sources_one_path() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = &server.base;
        for path in ["/pack-a/chat/completions", "/solo/chat/completions", "/lab/chat/completions"] {
            server.route(path, vec![Resp::sse(format!("data: {{\"text\":\"ok:{path}\"}}\n\ndata: [DONE]\n\n"))]);
        }

        write_user_plugin(&env, "wire-fixture", FIXTURE_WIRE_PLUGIN);
        write_user_plugin(
            &env,
            "pack-fixture",
            &format!(
                r#"return {{
  name = "pack-fixture",
  inject = {{ "llm" }},
  providers = {{ "pack-a", "pack-b" }},
  apply = function(ctx)
    ctx.llm:register_provider({{
      provider = "pack-a",
      wire = "fixture-chat",
      base_url = "{base}/pack-a",
      custom_tag = "from-pack",
      default_model = "m-pack",
      models = {{ {{ model_id = "m-pack", context_window = 64000, max_output = 4096 }} }},
    }})
    ctx.llm:register_provider({{
      provider = "pack-b",
      wire = "fixture-chat",
      base_url = "{base}/pack-b",
      default_model = "m-b",
      models = {{ {{ model_id = "m-b", context_window = 32000, max_output = 2048 }} }},
    }})
  end,
}}"#
            ),
        );
        write_user_plugin(
            &env,
            "vendor-solo",
            &format!(
                r#"return {{
  name = "vendor-solo",
  inject = {{ "llm" }},
  providers = {{ "solo" }},
  apply = function(ctx)
    ctx.llm:register_provider({{
      provider = "solo",
      wire = "fixture-chat",
      base_url = "{base}/solo",
      custom_tag = "from-solo",
      default_model = "m-solo",
      models = {{ {{ model_id = "m-solo", context_window = 128000, max_output = 8192 }} }},
    }})
  end,
}}"#
            ),
        );

        let h = env
            .host()
            .user_plugins()
            .config(json!({
                "providers": {
                    "lab": {
                        "wire": "fixture-chat",
                        "base_url": format!("{base}/lab"),
                        "custom_tag": "from-config",
                        "default_model": "m-lab",
                        "models": [{ "model_id": "m-lab", "context_window": 32000, "max_output": 4096 }]
                    }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        for (prov, model_ref, expected_origin, expected_tag, path) in [
            ("pack-a", "pack-a/m-pack", "plugin", "from-pack", "/pack-a/chat/completions"),
            ("solo", "solo/m-solo", "plugin", "from-solo", "/solo/chat/completions"),
            ("lab", "lab/m-lab", "config", "from-config", "/lab/chat/completions"),
        ] {
            let msg = ask(&h, model_ref).await;
            assert_eq!(text_of(&msg), format!("ok:{path}"), "{model_ref}: {msg}");
            let req = server.requests().into_iter().find(|r| r.path == path).unwrap_or_else(|| panic!("no req {path}"));
            assert_eq!(req.header("x-custom-tag"), Some(expected_tag), "{prov}");
            assert_eq!(req.json()["fixture_mode"], expected_tag, "{prov}: factory param rule must apply");
            let st_json: String =
                eval(&h, &format!("return mare.json.encode(require('llm.directory').status('{prov}'))")).await;
            let st: Value = serde_json::from_str(&st_json).unwrap();
            assert_eq!(st["state"], "registered", "{prov}: {st}");
            assert_eq!(st["origin"], expected_origin, "{prov}: {st}");
            assert_eq!(st["wire"], "fixture-chat", "{prov}: {st}");
        }

        h.shutdown().await;
    })
    .await
}

/// Data entries arriving BEFORE their wire plugin materialize automatically once the wire arrives,
/// and both the provider and its factory-created param rules are disposed when the wire leaves.
#[tokio::test]
async fn data_entry_load_order_independence_and_wire_disposal() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = &server.base;
        server.route("/late/chat/completions", vec![Resp::sse("data: {\"text\":\"late-ok\"}\n\ndata: [DONE]\n\n")]);

        // Load `aaa-vendor` BEFORE `zzz-wire` (alphabetical plugin discovery order).
        write_user_plugin(
            &env,
            "aaa-vendor",
            &format!(
                r#"return {{
  name = "aaa-vendor",
  inject = {{ "llm" }},
  providers = {{ "late" }},
  apply = function(ctx)
    ctx.llm:register_provider({{
      provider = "late",
      wire = "fixture-chat",
      base_url = "{base}/late",
      custom_tag = "late-tag",
      default_model = "m1",
      models = {{ {{ model_id = "m1", context_window = 32000, max_output = 4096 }} }},
    }})
  end,
}}"#
            ),
        );
        write_user_plugin(
            &env,
            "zzz-wire",
            &FIXTURE_WIRE_PLUGIN.replace(r#"name = "wire-fixture""#, r#"name = "zzz-wire""#),
        );

        let h = env.host().user_plugins().no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        // Registered even though `aaa-vendor` ran before `zzz-wire`.
        let msg = ask(&h, "late/m1").await;
        assert_eq!(text_of(&msg), "late-ok");
        assert_eq!(server.requests()[0].json()["fixture_mode"], "late-tag");

        // Now unregister the wire by reloading `zzz-wire` with an empty `apply`.
        let wire_file = env.config_dir().join("plugins/zzz-wire/init.lua");
        let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::write(
            &wire_file,
            r#"return { name = "zzz-wire", wires = { "fixture-chat" }, apply = function() end }"#,
        )
        .unwrap();
        std::fs::File::options().write(true).open(&wire_file).unwrap().set_modified(bumped).unwrap();
        h.kernel.reload(Some("zzz-wire")).await.unwrap();

        let (prov_nil, st_json, has_param): (bool, String, bool) = eval(
            &h,
            r#"
            local p = LLM:provider("late")
            local st = require("llm.directory").status("late")
            local defs = LLM:param_list({ id = "m1", provider = "late", ref = "late/m1", route = "chat" })
            local found = false
            for _, d in ipairs(defs) do
              if d.id == "fixture_mode" then found = true end
            end
            return p == nil, mare.json.encode(st), found
            "#,
        )
        .await;
        assert!(prov_nil, "provider must be removed when its wire is disposed");
        assert!(!has_param, "factory-created param rule must be disposed when wire is disposed");
        let st: Value = serde_json::from_str(&st_json).unwrap();
        assert_eq!(st["state"], "unresolved", "{st}");

        h.shutdown().await;
    })
    .await
}

/// Config overlay merges fields and replaces `models`, `replace = true` stacks and restores on
/// dispose, and a code provider receiving config `wire`/`wire_api` reports a doctor problem.
#[tokio::test]
async fn config_overlay_replace_stack_and_code_provider_wire_warning() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = &server.base;
        server.route("/overlaid/chat/completions", vec![Resp::sse("data: {\"text\":\"overlay-ok\"}\n\ndata: [DONE]\n\n")]);

        write_user_plugin(&env, "wire-fixture", FIXTURE_WIRE_PLUGIN);
        write_user_plugin(
            &env,
            "vendor-overlay",
            &format!(
                r#"return {{
  name = "vendor-overlay",
  inject = {{ "llm" }},
  providers = {{ "ov" }},
  apply = function(ctx)
    ctx.llm:register_provider({{
      provider = "ov",
      wire = "fixture-chat",
      base_url = "{base}/original",
      custom_tag = "orig-tag",
      models = {{ {{ model_id = "orig-model", context_window = 16000, max_output = 2048 }} }},
    }})
  end,
}}"#
            ),
        );
        write_user_plugin(
            &env,
            "code-prov",
            r#"return {
  name = "code-prov",
  inject = { "llm" },
  providers = { "coded" },
  apply = function(ctx)
    ctx.llm:register_provider({
      provider = "coded",
      is_configured = function() return true end,
      stream = function() return { role = "assistant", content = {}, stop_reason = "stop", usage = {} } end,
      models = { { model_id = "c1", context_window = 32000, max_output = 4096 } },
    })
  end,
}"#,
        );

        let h = env
            .host()
            .user_plugins()
            .config(json!({
                "providers": {
                    "ov": {
                        "base_url": format!("{base}/overlaid"),
                        "custom_tag": "overlay-tag",
                        "models": [{ "model_id": "new-model", "context_window": 64000, "max_output": 4096 }]
                    },
                    "coded": {
                        "wire_api": "responses"
                    }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        // Overlay replaced base_url, custom_tag, and the entire models seed list.
        let (has_new, has_orig_static): (bool, bool) = eval(
            &h,
            r#"
            local found_new, found_orig = false, false
            for _, m in ipairs(LLM:models()) do
              if m.ref == "ov/new-model" then found_new = true end
              if m.ref == "ov/orig-model" then found_orig = true end
            end
            return found_new, found_orig
            "#,
        )
        .await;
        assert!(has_new && !has_orig_static, "config overlay must replace models list: new={has_new}, orig={has_orig_static}");
        let msg = ask(&h, "ov/new-model").await;
        assert_eq!(text_of(&msg), "overlay-ok");
        assert_eq!(server.requests()[0].header("x-custom-tag"), Some("overlay-tag"));

        // `replace = true` stacks on top and restores lower data entry when disposed.
        let (during_replace, after_dispose): (String, String) = eval(
            &h,
            r#"
            local rm = LLM:register_provider({
              provider = "ov",
              replace = true,
              is_configured = function() return true end,
              stream = function() return { role = "assistant", content = { { type = "text", text = "top" } }, stop_reason = "stop", usage = {} } end,
              models = { { model_id = "top-model", context_window = 8192, max_output = 1024 } },
            })
            local during = LLM:resolve("ov/top-model") and "top" or "none"
            rm()
            local after = LLM:resolve("ov/new-model") and "restored" or "missing"
            return during, after
            "#,
        )
        .await;
        assert_eq!((during_replace.as_str(), after_dispose.as_str()), ("top", "restored"));

        // Code provider receiving config `wire_api` must report a doctor problem.
        let probs_json: String = eval(&h, "return mare.json.encode(mare.plugins.status('code-prov').problems)").await;
        let probs: Vec<Value> = serde_json::from_str(&probs_json).unwrap();
        assert!(
            probs.iter().any(|p| p["key"].as_str() == Some("config-wire:coded")),
            "expected config-wire problem on code-prov: {probs:?}"
        );

        h.shutdown().await;
    })
    .await
}

/// Installing a `mare.fs.read`/`stat` spy via an early-applying user plugin (and across a full
/// `kernel.reload(None)` pass while the spy is already active from byte 0) must see ZERO reads of
/// `auth.json` or `models-cache/*.json` during plugin `apply()` (including `statusline` apply ->
/// `layout.install` -> `segments.lua` -> `llm:current()` -> `resolve()` with `state.model` set to
/// a listed model). Then, without `ui/ready`, a headless first request (`request/prepare` +
/// `llm:stream`) for a seed model must immediately observe the disk-cached `context_window`
/// override.
#[tokio::test]
async fn apply_does_not_read_catalog_or_auth_and_headless_first_request_hydrates_seed() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("DEEPSEEK_API_KEY", "test-deepseek");
        let server = MockServer::start().await;
        server.route("/compat", vec![Resp::sse(fixture("openai-compat.sse"))]);

        let cache_dir = env.home.path().join("state/models-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            cache_dir.join("deepseek.json"),
            r#"{"fetched_at": 9999999999, "models": [{"id": "cached-from-disk", "context_window": 99000}, {"id": "deepseek-chat", "context_window": 42000}]}"#,
        )
        .unwrap();
        let auth_path = env.home.path().join("state/auth.json");
        std::fs::write(&auth_path, r#"{"version":1}"#).unwrap();
        let state_path = env.home.path().join("state/state.json");
        std::fs::write(&state_path, r#"{"model":"deepseek/cached-from-disk"}"#).unwrap();

        // Install a user plugin whose `apply()` installs the spy before `statusline` / `provider-*`
        // finish applying, and ALSO verify a full `h.reload()` pass while the spy is already active
        // before ANY plugin's `apply()` runs (capturing `llm`, all `provider-*`, and `statusline`).
        write_user_plugin(
            &env,
            "aaa-fs-spy",
            r#"
return {
  name = "aaa-fs-spy",
  apply = function()
    if mare.fs._fs_spy then return end
    local reads = {}
    local orig_read, orig_stat = mare.fs.read, mare.fs.stat
    mare.fs._fs_spy = { reads = reads, orig_read = orig_read, orig_stat = orig_stat }
    local function record(op, p)
      if type(p) == "string" and (p:find("models-cache", 1, true) or p:find("auth.json", 1, true)) then
        mare.fs._fs_spy.reads[#mare.fs._fs_spy.reads + 1] = op .. ":" .. p
      end
    end
    mare.fs.read = function(p, ...)
      record("read", p)
      return orig_read(p, ...)
    end
    mare.fs.stat = function(p, ...)
      record("stat", p)
      return orig_stat(p, ...)
    end
  end,
}
"#,
        );

        let h = env
            .host()
            .user_plugins()
            .no_session()
            .config(json!({
                "providers": {
                    "deepseek": { "base_url": format!("{}/compat", server.base) }
                },
                "plugins": {
                    "llm": { "catalog_refresh_ms": 86400000 }
                }
            }))
            .build()
            .await;

        // Also run a full reload with the spy already active BEFORE `llm`, `provider-*`, and
        // `statusline` run `apply()`, so every single plugin's `apply()` is observed from byte 0.
        let boot_reads: Vec<String> = eval(
            &h,
            r#"
            local spy = assert(mare.fs._fs_spy, "aaa-fs-spy must have installed spy")
            local out = {}
            for _, r in ipairs(spy.reads) do out[#out + 1] = r end
            spy.reads = {}
            mare.g["llm.startup_done"] = nil
            return out
            "#,
        )
        .await;
        h.kernel.reload(None).await.unwrap();
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (reload_reads, seed_before, seed_after_prepare, stream_ctx): (Vec<String>, i64, i64, i64) = eval(
            &h,
            r#"
            local spy = assert(mare.fs._fs_spy)
            local reload_reads = {}
            for _, r in ipairs(spy.reads) do
              reload_reads[#reload_reads + 1] = r
            end
            mare.fs.read = spy.orig_read
            mare.fs.stat = spy.orig_stat
            mare.fs._fs_spy = nil

            -- Seed model before request/prepare has static 128000 (resolve does no blocking I/O).
            local before_m = assert(LLM:resolve("deepseek/deepseek-chat"))
            local seed_before = before_m.context_window

            -- Headless first request (no ui/ready!): prepare_request barrier awaits disk cache for
            -- current provider, so seed model immediately receives cached context_window = 42000.
            local args = { model = before_m }
            LLM:prepare_request(args)
            local after_m = assert(LLM:resolve("deepseek/deepseek-chat"))
            local msg = LLM:stream({
              model = before_m,
              messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            }, function() end)
            assert(msg.stop_reason ~= "error", tostring(msg.error))
            return reload_reads, seed_before, after_m.context_window, args.model.context_window
            "#,
        )
        .await;

        assert!(
            boot_reads.is_empty(),
            "initial startup apply must not read auth.json or models-cache: {boot_reads:?}"
        );
        assert!(
            reload_reads.is_empty(),
            "full reload apply (llm + provider-* + statusline render) must not read auth.json or models-cache: {reload_reads:?}"
        );
        assert_eq!(seed_before, 128000, "resolve must not block on disk I/O");
        assert_eq!(
            seed_after_prepare, 42000,
            "headless request/prepare barrier must hydrate seed model's cached context_window"
        );
        assert_eq!(
            stream_ctx, 42000,
            "args.model in request/prepare must also see the hydrated seed model's context_window"
        );

        h.shutdown().await;
    })
    .await
}

/// Lazy hydration must be single-flight per provider (`with_catalog_lock`),
/// check `refresh_gen` against concurrent `invalidate`, emit `llm/providers-changed`, and log
/// failures without retrying endlessly.
#[tokio::test]
async fn lazy_hydration_is_single_flight_checks_refresh_gen_and_logs_errors() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("DEEPSEEK_API_KEY", "test-deepseek");
        let cache_dir = env.home.path().join("state/models-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            cache_dir.join("deepseek.json"),
            r#"{"fetched_at": 9999999999, "models": [{"id": "cached-sf", "context_window": 77000}]}"#,
        )
        .unwrap();

        let h = env.host().no_session().build_exposed().await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        // 1. Two concurrent fibers calling prepare_request (blocking hydration) + models/resolve
        //    while the disk read is suspended on a channel must read disk ONCE and both observe the
        //    hydrated model, and emit `llm/providers-changed`.
        let (read_count, both_loaded, changed_events, stale_ignored): (i64, bool, i64, bool) = eval(
            &h,
            r#"
            local changed = 0
            C.cordis:on("llm/providers-changed", function() changed = changed + 1 end)

            local orig_read = mare.fs.read
            local count = 0
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            mare.fs.read = function(p, ...)
              if type(p) == "string" and p:find("models-cache/deepseek.json", 1, true) then
                count = count + 1
                if count == 1 then
                  entered_tx:send(true)
                  release_rx:recv()
                end
              end
              return orig_read(p, ...)
            end

            local d1_tx, d1_rx = mare.task.channel()
            local d2_tx, d2_rx = mare.task.channel()
            mare.task.spawn(function()
              local args = { model = LLM:resolve("deepseek/cached-sf") }
              LLM:prepare_request(args)
              local m = LLM:resolve("deepseek/cached-sf")
              d1_tx:send(m ~= nil and m.listed == true and m.context_window == 77000)
            end)
            local _, entered = mare.task.select(function()
              return entered_rx:recv()
            end, function()
              mare.task.sleep(1500)
              return false
            end)
            assert(entered == true, "prepare_request did not trigger single-flight disk hydration")
            mare.task.spawn(function()
              -- Second concurrent caller while first read is in flight
              local _ = LLM:models()
              local args = { model = LLM:resolve("deepseek/cached-sf") }
              LLM:prepare_request(args)
              local m = LLM:resolve("deepseek/cached-sf")
              d2_tx:send(m ~= nil and m.listed == true and m.context_window == 77000)
            end)
            local function recv_or_timeout(rx, label)
              local _, v = mare.task.select(function()
                return rx:recv()
              end, function()
                mare.task.sleep(3000)
                error("timeout waiting for " .. label, 0)
              end)
              return v
            end
            mare.task.sleep(30)
            release_tx:send(true)
            local ok1 = recv_or_timeout(d1_rx, "d1")
            local ok2 = recv_or_timeout(d2_rx, "d2")

            -- 2. Race with invalidate (refresh_gen bump): register a fresh provider with a pending
            --    cache, suspend its hydrate read, trigger auth_set (bumps refresh_gen), then resume
            --    and verify the stale disk payload is NOT written back.
            mare.fs.write(
              mare.paths.state .. "/models-cache/raceprov.json",
              '{"fetched_at": 9999999999, "models": [{"id": "stale-from-old-gen"}]}'
            )
            local rm = LLM:register_provider({
              provider = "raceprov",
              replace = true,
              is_configured = function() return true end,
              stream = function() return { role = "assistant", content = {}, stop_reason = "stop", usage = {} } end,
              catalog_scope = function() return "same-scope" end,
              catalog = function() return { url = "http://127.0.0.1:1/models", decode = function() return {} end } end,
              models = { { model_id = "seed-r", context_window = 32000, max_output = 4096 } },
            })
            local r_ent_tx, r_ent_rx = mare.task.channel()
            local r_rel_tx, r_rel_rx = mare.task.channel()
            local r_reads = 0
            mare.fs.read = function(p, ...)
              if type(p) == "string" and p:find("models-cache/raceprov.json", 1, true) then
                r_reads = r_reads + 1
                if r_reads == 1 then
                  r_ent_tx:send(true)
                  r_rel_rx:recv()
                  return '{"fetched_at": 9999999999, "models": [{"id": "stale-from-old-gen"}]}'
                end
              end
              return orig_read(p, ...)
            end
            local race_done_tx, race_done_rx = mare.task.channel()
            mare.task.spawn(function()
              LLM:prepare_request({ model = LLM:resolve("raceprov/seed-r") })
              race_done_tx:send(true)
            end)
            local _, r_entered = mare.task.select(function()
              return r_ent_rx:recv()
            end, function()
              mare.task.sleep(1500)
              return false
            end)
            assert(r_entered == true, "raceprov prepare_request did not enter disk read")
            -- Bump refresh_gen via invalidate (same path as llm/login -> invalidate_catalog)
            mare.fs.write(mare.paths.state .. "/models-cache/raceprov.json", '{"fetched_at": 9999999999, "models": []}')
            LLM:invalidate_catalog("raceprov")
            r_rel_tx:send(true)
            recv_or_timeout(race_done_rx, "race_done")
            local stale_m = LLM:resolve("raceprov/stale-from-old-gen")
            local stale_ignored = (stale_m == nil or stale_m.listed ~= true)
            rm()
            mare.fs.read = orig_read

            return count, (ok1 and ok2), changed, stale_ignored
            "#,
        )
        .await;

        assert_eq!(read_count, 1, "concurrent hydration must read disk cache only once");
        assert!(both_loaded, "both concurrent fibers must see the hydrated model");
        assert!(changed_events >= 1, "successful hydration must emit llm/providers-changed");
        assert!(stale_ignored, "hydration with stale refresh_gen must not write back old models");

        h.shutdown().await;
    })
    .await
}

/// RPC-origin provider replaces a lower-layer entry on the stack; disconnecting
/// (disposing the RPC fiber) removes the RPC entry and restores the lower-layer entry.
#[tokio::test]
async fn rpc_provider_disconnect_removes_entry_and_restores_lower_stack() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build_exposed().await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (rpc_origin, rpc_name, restored_origin, restored_name): (String, String, String, String) = eval(
            &h,
            r#"
            -- Install a base (lower-stack) provider
            local rm_base = LLM:register_provider({
              provider = "rpc-stack",
              replace = true,
              default_model = "m1",
              is_configured = function() return true end,
              models = { { model_id = "m1", name = "Base Layer" } },
              stream = function() return { role = "assistant", content = {}, stop_reason = "stop", usage = {} } end,
            })

            -- Register an RPC-origin replacement using the real `rpc:connect(id)` connection fiber
            local rpc_svc = assert(C.cordis:get("rpc"), "rpc service must exist")
            rpc_svc:connect("test-rpc-conn-1")
            local rpc_ctx = assert(rpc_svc:context("test-rpc-conn-1"))
            rpc_ctx:get("llm"):register_provider({
              provider = "rpc-stack",
              replace = true,
              default_model = "m1",
              models = { { model_id = "m1", name = "RPC Override" } },
              stream = function() return { role = "assistant", content = {}, stop_reason = "stop", usage = {} } end,
            })

            local st1 = assert(require("llm.directory").status("rpc-stack"))
            local m1 = assert(LLM:resolve("rpc-stack/m1"))

            -- Simulate RPC disconnect via `rpc:disconnect(id)` (await returned Cordis future)
            local disc = rpc_svc:disconnect("test-rpc-conn-1")
            if type(disc) == "table" and disc.await then
              disc:await()
            end

            local st2 = assert(require("llm.directory").status("rpc-stack"))
            local m2 = assert(LLM:resolve("rpc-stack/m1"))
            rm_base()
            return tostring(st1.origin), tostring(m1.name), tostring(st2.origin), tostring(m2.name)
            "#,
        )
        .await;

        assert_eq!(rpc_origin, "rpc");
        assert_eq!(rpc_name, "RPC Override");
        assert_ne!(restored_origin, "rpc");
        assert_eq!(restored_name, "Base Layer");

        h.shutdown().await;
    })
    .await
}

/// Wire factory failure reporting, pack unload cleanup, wire re-registration, and strict validation:
/// - config/data entry whose wire plugin fails during `apply()` reports `state = "failed"` with `f.summary`;
/// - vendor pack unload disposes BOTH the provider and its wire factory's `params` rule;
/// - wire re-registration swaps already-materialized data entries onto the new wire's factory hooks;
/// - `validate.data_entry` reports unknown fields even when a wire declares no `provider_fields`.
#[tokio::test]
async fn wire_apply_failure_pack_unload_params_and_wire_reregister() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/pack/chat/completions", vec![Resp::sse("data: {\"text\":\"ok\"}\n\ndata: [DONE]\n\n")]);

        write_user_plugin(&env, "wire-fixture", FIXTURE_WIRE_PLUGIN);
        write_user_plugin(
            &env,
            "wire-broken",
            r#"
return {
  name = "wire-broken",
  inject = { "llm" },
  wires = { "broken-wire" },
  apply = function()
    error("wire-broken boom during apply")
  end,
}
"#,
        );
        write_user_plugin(
            &env,
            "wire-plain",
            r#"
return {
  name = "wire-plain",
  inject = { "llm" },
  wires = { "plain-wire" },
  apply = function(ctx)
    ctx.llm:register_wire({
      name = "plain-wire",
      route = "chat",
      path = "/chat",
      encode = function(req, ep) return { method = "POST", url = ep.url, headers = {}, body = { model = req.model.id } } end,
      decoder = function() return { event = function() end, finish = function() return { role = "assistant", content = {}, stop_reason = "stop", usage = {} } end } end,
      provider = function(name, entry)
        return {
          route = "chat",
          wire = "plain-wire",
          is_configured = function() return true end,
          stream = function() return { role = "assistant", content = {}, stop_reason = "stop", usage = {} } end,
        }
      end,
    })
  end,
}
"#,
        );
        write_user_plugin(
            &env,
            "vendor-typo",
            &format!(
                r#"
return {{
  name = "vendor-typo",
  inject = {{ "llm" }},
  providers = {{ "typo-prov" }},
  apply = function(ctx)
    ctx.llm:register_provider({{
      provider = "typo-prov",
      wire = "plain-wire",
      base_url = "{}/typo",
      bogus_extra_field = 123,
      models = {{ {{ model_id = "t1" }} }},
    }})
  end,
}}
"#,
                server.base
            ),
        );

        let h = env
            .host()
            .user_plugins()
            .no_session()
            .config(json!({
                "providers": {
                    "broken-cfg": {
                        "wire": "broken-wire",
                        "base_url": format!("{}/broken", server.base),
                        "models": [{ "model_id": "b1" }]
                    },
                    "swap-cfg": {
                        "wire": "fixture-chat",
                        "base_url": format!("{}/pack", server.base),
                        "models": [{ "model_id": "s1", "reasoning": true, "efforts": ["low", "high"] }]
                    }
                }
            }))
            .build_unchecked_exposed()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        // 1. Config entry on failed wire plugin reports state = "failed" with f.summary
        let (broken_state, broken_plugin, broken_reason): (String, String, String) = eval(
            &h,
            r#"
            local st = assert(require("llm.directory").status("broken-cfg"))
            return tostring(st.state), tostring(st.plugin), tostring(st.reason)
            "#,
        )
        .await;
        assert_eq!(broken_state, "failed");
        assert_eq!(broken_plugin, "wire-broken");
        assert!(
            broken_reason.contains("wire-broken boom during apply"),
            "failed wire reason must include failure summary: {broken_reason}"
        );

        // 2. Unknown field on a wire WITHOUT provider_fields must be reported as doctor problem
        let typo_probs_json: String = eval(
            &h,
            r#"
            local st = mare.plugins.status("vendor-typo")
            return mare.json.encode(st and st.problems or {})
            "#,
        )
        .await;
        let typo_probs: Vec<Value> = serde_json::from_str(&typo_probs_json).unwrap();
        assert!(
            typo_probs.iter().any(|p| p["message"].as_str().unwrap_or("").contains("bogus_extra_field")),
            "unknown field must be reported even when wire omits provider_fields: {typo_probs:?}"
        );

        // 3. Vendor pack unload disposes BOTH provider and factory params rule
        let (param_before, param_after, provider_after): (bool, bool, bool) = eval(
            &h,
            &format!(
                r#"
                local pack_fiber = C.cordis:plugin({{ name = "pack-unload-plugin", apply = function() end }})
                pack_fiber.ctx:get("llm"):register_provider({{
                  provider = "pack-unload",
                  replace = true,
                  wire = "fixture-chat",
                  base_url = "{}/pack",
                  custom_tag = "fast",
                  models = {{ {{ model_id = "p1", reasoning = true, efforts = {{ "low", "high" }} }} }},
                }})
                local m1 = assert(LLM:resolve("pack-unload/p1"))
                local req1 = {{ model = m1, body = {{}} }}
                LLM:apply_params(req1, {{ body = req1.body, headers = {{}} }})
                local has_before = req1.body.fixture_mode == "fast"

                local d = pack_fiber:dispose()
                if type(d) == "table" and d.await then d:await() end

                -- Re-register a bare provider with the same name (no factory params) to prove the
                -- old factory's params rule was disposed and does not leak onto the same name.
                local rm = LLM:register_provider({{
                  provider = "pack-unload",
                  replace = true,
                  route = "chat",
                  is_configured = function() return true end,
                  stream = function() return {{ role = "assistant", content = {{}}, stop_reason = "stop", usage = {{}} }} end,
                  models = {{ {{ model_id = "p1", reasoning = true, efforts = {{ "low", "high" }} }} }},
                }})
                local m2 = assert(LLM:resolve("pack-unload/p1"))
                local req = {{ model = m2, body = {{}} }}
                LLM:apply_params(req, {{ body = req.body, headers = {{}} }})
                local leaked = req.body.fixture_mode ~= nil
                rm()
                local gone = LLM:provider("pack-unload") == nil
                return has_before, not leaked, gone
                "#,
                server.base
            ),
        )
        .await;
        assert!(param_before, "factory params rule must be active while pack is loaded");
        assert!(param_after, "unloading vendor pack must dispose factory params rule");
        assert!(provider_after, "unloaded provider must be removed");

        // 4. Wire re-registration swaps already-materialized data entries onto the new wire's hooks
        let swapped_label: String = eval(
            &h,
            r#"
            local f = C.cordis:plugin({ name = "wire-swap-v2", apply = function() end })
            f.ctx:get("llm"):register_wire({
              name = "fixture-chat",
              route = "chat",
              path = "/chat/completions",
              encode = function(req, ep) return { method = "POST", url = ep.url, headers = {}, body = { model = req.model.id } } end,
              decoder = function() return { event = function() end, finish = function() return { role = "assistant", content = {}, stop_reason = "stop", usage = {} } end } end,
              provider = function(name, entry)
                return {
                  route = "chat",
                  wire = "fixture-chat",
                  label = "Swapped Wire V2",
                  is_configured = function() return true end,
                  stream = function() return { role = "assistant", content = {}, stop_reason = "stop", usage = {} } end,
                }
              end,
            })
            local p = assert(LLM:provider("swap-cfg"))
            local lbl = tostring(p.label)
            local d = f:dispose()
            if type(d) == "table" and d.await then d:await() end
            return lbl
            "#,
        )
        .await;
        assert_eq!(
            swapped_label, "Swapped Wire V2",
            "re-registering wire of same name must re-materialize existing data entries with new hooks"
        );

        h.shutdown().await;
    })
    .await
}

/// Directory problems must be emitted on `kernel/doctor`, valid prompt-cache capability flags must
/// not trigger unknown-field warnings, and unknown fields introduced by a user config overlay onto
/// a plugin-owned data entry must be attributed to user config (doctor) rather than the plugin.
#[tokio::test]
async fn directory_problems_reach_kernel_doctor_and_overlay_typos_blame_user_config() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = &server.base;

        write_user_plugin(&env, "wire-fixture", FIXTURE_WIRE_PLUGIN);
        write_user_plugin(
            &env,
            "pack-overlay-test",
            &format!(
                r#"
return {{
  name = "pack-overlay-test",
  inject = {{ "llm" }},
  providers = {{ "pack-ov" }},
  apply = function(ctx)
    ctx.llm:register_provider({{
      provider = "pack-ov",
      wire = "fixture-chat",
      base_url = "{base}/pack-ov",
      models = {{ {{ model_id = "m1" }} }},
    }})
  end,
}}
"#
            ),
        );

        let h = env
            .host()
            .user_plugins()
            .no_session()
            .config(json!({
                "providers": {
                    "bad-user": {
                        "wire": "fixture-chat",
                        "base_url": format!("{base}/bad-user"),
                        "typo_user": true
                    },
                    "pack-ov": {
                        "typo_overlay": true
                    },
                    "cache-flags-ok": {
                        "wire": "openai-responses",
                        "base_url": format!("{base}/cache-flags"),
                        "supports_explicit_prompt_cache_mode": true,
                        "supports_in_memory_cache_retention": true
                    }
                }
            }))
            .build_exposed()
            .await;

        let (failed_names, pack_probs): (Vec<String>, Vec<String>) = eval(
            &h,
            r#"
            local checks = {}
            C.cordis:get("bus"):emit("kernel/doctor", checks)
            local failed = {}
            for _, c in ipairs(checks) do
              if c.ok == false and type(c.name) == "string" and c.name:find("^provider directory") then
                failed[#failed + 1] = c.name .. ":" .. tostring(c.detail or "")
              end
            end
            table.sort(failed)
            local pp = {}
            for _, p in ipairs(mare.plugins.status("pack-overlay-test").problems or {}) do
              pp[#pp + 1] = tostring(p.key or "") .. ":" .. tostring(p.message or "")
            end
            return failed, pp
            "#,
        )
        .await;

        assert!(
            failed_names.iter().any(|s| s.contains("bad-user") && s.contains("typo_user")),
            "kernel/doctor must report unknown field on user config entry: {failed_names:?}"
        );
        assert!(
            failed_names.iter().any(|s| s.contains("pack-ov") && s.contains("typo_overlay")),
            "kernel/doctor must report unknown field from user overlay on plugin preset: {failed_names:?}"
        );
        assert!(
            !failed_names.iter().any(|s| s.contains("cache-flags-ok")),
            "valid prompt-cache capability fields must not trigger doctor problems: {failed_names:?}"
        );
        assert!(
            pack_probs.is_empty(),
            "user overlay typo must NOT be attributed to the preset's owning plugin: {pack_probs:?}"
        );

        h.shutdown().await;
    })
    .await
}

/// After `kernel.reload()`, `llm` must restore its post-startup ready state via `kernel/reloaded`
/// so `current()` uses its configured-provider fallback and `resolve()`/`models()` still schedule
/// background disk catalog hydration.
#[tokio::test]
async fn reload_restores_startup_done_for_current_and_background_hydration() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("DEEPSEEK_API_KEY", "test-deepseek");
        let cache_dir = env.home.path().join("state/models-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            cache_dir.join("deepseek.json"),
            r#"{"fetched_at": 9999999999, "models": [{"id": "cached-after-reload", "context_window": 55000}]}"#,
        )
        .unwrap();

        let h = env.host().config(json!({ "model": "" })).no_session().build().await;
        h.kernel.reload(None).await.unwrap();
        global(&h, "LLM", h.service_table("llm").unwrap());

        let cur_provider: String = eval(
            &h,
            r#"
            local c = LLM:current()
            local _ = LLM:models()
            return c and tostring(c.provider) or ""
            "#,
        )
        .await;
        assert_eq!(
            cur_provider, "deepseek",
            "after reload, current() must still select the configured provider instead of skipping is_configured()"
        );

        wait_until(WAIT_SECS, "background catalog hydration after reload", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"
                    local m = LLM:resolve("deepseek/cached-after-reload")
                    return m ~= nil and m.listed == true and m.context_window == 55000
                    "#,
                )
                .await
            }
        })
        .await;

        h.shutdown().await;
    })
    .await
}

/// `llm:stream()` must preserve the caller-supplied `req.model` (custom fields and frozen snapshots)
/// when the provider's disk catalog was NOT newly hydrated by this call.
#[tokio::test]
async fn stream_preserves_caller_model_and_snapshot_when_already_hydrated() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = &server.base;
        server.route("/keep-model/chat/completions", vec![Resp::sse("data: {\"text\":\"ok\"}\n\ndata: [DONE]\n\n")]);

        write_user_plugin(
            &env,
            "wire-inspect-model",
            r#"
return {
  name = "wire-inspect-model",
  inject = { "llm" },
  wires = { "inspect-chat" },
  apply = function(ctx)
    local llm = ctx.llm
    llm:register_wire({
      name = "inspect-chat",
      route = "chat",
      path = "/chat/completions",
      encode = function(req, ep)
        mare.g.seen_model_tag = req.model and req.model._caller_tag or ""
        mare.g.seen_model_max = req.model and req.model.max_output or 0
        return {
          method = "POST",
          url = ep.url,
          headers = { accept = "text/event-stream" },
          body = { model = req.model.id, max_tokens = req.model.max_output },
          model = req.model.id,
        }
      end,
      decoder = function()
        return {
          event = function() end,
          finish = function()
            return { role = "assistant", content = { { type = "text", text = "ok" } }, stop_reason = "stop", usage = {} }
          end,
        }
      end,
      provider = function(name, entry)
        local stream = llm:wire("inspect-chat")
        local function ep()
          return {
            provider = name,
            label = name,
            base_url = entry.base_url,
            url = entry.base_url .. "/chat/completions",
          }
        end
        return {
          route = "chat",
          wire = "inspect-chat",
          endpoint = ep,
          stream = function(req, sink)
            return stream(ep, req, sink)
          end,
          no_key = true,
          is_configured = function()
            return true
          end,
        }
      end,
    })
  end,
}
"#,
        );

        let h = env
            .host()
            .user_plugins()
            .no_session()
            .config(json!({
                "providers": {
                    "keep-model": {
                        "wire": "inspect-chat",
                        "base_url": format!("{base}/keep-model"),
                        "models": [{ "model_id": "m1", "max_output": 8192 }]
                    }
                }
            }))
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (seen_tag, seen_max): (String, i64) = eval(
            &h,
            r#"
            local base_m = assert(LLM:resolve("keep-model/m1"))
            local custom_m = table.copy(base_m)
            custom_m.max_output = 1234
            custom_m._caller_tag = "frozen-snapshot"
            local msg = LLM:stream({
              model = custom_m,
              messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            }, function() end)
            assert(msg.stop_reason ~= "error", tostring(msg.error))
            return tostring(mare.g.seen_model_tag or ""), tonumber(mare.g.seen_model_max or 0)
            "#,
        )
        .await;

        assert_eq!(seen_tag, "frozen-snapshot", "stream must not clobber custom caller model fields");
        assert_eq!(seen_max, 1234, "stream must preserve caller model max_output instead of overwriting via resolve()");

        h.shutdown().await;
    })
    .await
}

/// Wire quirks follow `entry.dialect` (not the user's provider name).
/// - Overlaying shipped `deepseek` keeps `dialect = "deepseek"` -> sends `thinking.type`.
/// - Custom `ds2` without `dialect` does NOT send `thinking.type`.
/// - Custom `ds3` with `dialect = "deepseek"` DOES send `thinking.type`.
/// - Replacing `deepseek` with a plain entry (no `dialect`, non-deepseek host) does NOT send
///   `thinking.type` just because its provider name is `"deepseek"`.
#[tokio::test]
async fn dialect_follows_entry_not_name() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("DEEPSEEK_API_KEY", "test-ds");
        let server = MockServer::start().await;
        let base = &server.base;
        let ok_sse = compat_events(&[compat_text("ok"), compat_stop()]);
        for path in ["/ds-overlay", "/ds2", "/ds3", "/ds-plain-replace"] {
            server.route(path, vec![Resp::sse(ok_sse.clone())]);
        }

        let h = env
            .host()
            .config(json!({
                "providers": {
                    "deepseek": { "base_url": format!("{base}/ds-overlay") },
                    "ds2": {
                        "base_url": format!("{base}/ds2"),
                        "no_key": true,
                        "models": [{ "model_id": "m1", "reasoning": true, "efforts": ["off", "high"] }]
                    },
                    "ds3": {
                        "base_url": format!("{base}/ds3"),
                        "no_key": true,
                        "dialect": "deepseek",
                        "models": [{ "model_id": "m1", "reasoning": true, "efforts": ["off", "high"] }]
                    }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        async fn stream_high(h: &Host, model_ref: &str) -> Value {
            let v: mlua::Value = h
                .lua()
                .load(
                    r#"local ref = ...
                    local m = assert(LLM:resolve(ref), "unknown " .. ref)
                    return LLM:stream({ model = m, effort = "high",
                        messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)"#,
                )
                .into_function()
                .unwrap()
                .call_async::<mlua::Value>(model_ref.to_string())
                .await
                .unwrap();
            mare_shared::lua::lua_to_json(h.lua(), v).unwrap()
        }

        let _ = stream_high(&h, "deepseek/deepseek-reasoner").await;
        let _ = stream_high(&h, "ds2/m1").await;
        let _ = stream_high(&h, "ds3/m1").await;

        let req_by_path = |p: &str| {
            server
                .requests()
                .into_iter()
                .find(|r| r.path.starts_with(p))
                .unwrap_or_else(|| panic!("missing request for {p}"))
        };
        assert_eq!(
            req_by_path("/ds-overlay").json()["thinking"]["type"],
            "enabled",
            "shipped deepseek overlay must keep deepseek dialect"
        );
        assert!(
            req_by_path("/ds2").json().get("thinking").is_none(),
            "custom ds2 without dialect must NOT send deepseek thinking"
        );
        assert_eq!(
            req_by_path("/ds3").json()["thinking"]["type"],
            "enabled",
            "custom ds3 with dialect='deepseek' must send deepseek thinking"
        );

        h.shutdown().await;
        drop(env);

        // Replacing `deepseek` with a plain data entry (no dialect, non-deepseek host, no
        // config overlay) must NOT infer deepseek thinking from the name `"deepseek"`.
        let env2 = TestEnv::new().await;
        let h2 = env2.host().no_session().build().await;
        global(&h2, "LLM", h2.service_table("llm").unwrap());
        exec(
            &h2,
            &format!(
                r#"
                LLM:register_provider({{
                    provider = "deepseek",
                    replace = true,
                    wire = "openai-chat",
                    base_url = "{base}/ds-plain-replace",
                    no_key = true,
                    models = {{ {{ model_id = "plain-r", reasoning = true, efforts = {{ "off", "high" }} }} }},
                }})
                "#
            ),
        )
        .await;
        let _ = stream_high(&h2, "deepseek/plain-r").await;
        assert!(
            req_by_path("/ds-plain-replace").json().get("thinking").is_none(),
            "plain replacement named 'deepseek' without dialect must NOT send deepseek thinking"
        );

        h2.shutdown().await;
    })
    .await
}

/// `mare.provider("<preset>", { default_model, models, ... })` overlays fields onto shipped
/// presets (and replaces the entire seed list) instead of being skipped.
#[tokio::test]
async fn preset_overlay_fields_take_effect() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = &server.base;
        server.route("/ollama-ov", vec![Resp::sse(compat_events(&[compat_text("ollama-ok"), compat_stop()]))]);

        let h = env
            .host()
            .config(json!({
                "providers": {
                    "ollama": {
                        "base_url": format!("{base}/ollama-ov"),
                        "default_model": "qwen3",
                        "models": [{ "model_id": "qwen3", "context_window": 64000, "max_output": 4096 }]
                    },
                    "deepseek": {
                        "models": [{ "model_id": "custom-ds", "context_window": 32000, "max_output": 4096 }]
                    }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (ollama_def, has_qwen3, has_custom_ds, has_old_ds): (String, bool, bool, bool) = eval(
            &h,
            r#"
            local op = LLM:provider("ollama")
            local has_q, has_c, has_old = false, false, false
            for _, m in ipairs(LLM:models()) do
              if m.ref == "ollama/qwen3" then has_q = true end
              if m.ref == "deepseek/custom-ds" then has_c = true end
              if m.ref == "deepseek/deepseek-chat" then has_old = true end
            end
            return tostring(op and op.default_model or ""), has_q, has_c, has_old
            "#,
        )
        .await;
        assert_eq!(ollama_def, "qwen3");
        assert!(has_qwen3, "ollama/qwen3 must be seeded by config overlay");
        assert!(
            has_custom_ds && !has_old_ds,
            "deepseek overlay must replace seeds: custom={has_custom_ds}, old={has_old_ds}"
        );

        let msg = ask(&h, "ollama/qwen3").await;
        assert_eq!(text_of(&msg), "ollama-ok");

        h.shutdown().await;
    })
    .await
}

/// `mare.provider("deepseek", { wire_api = "responses", base_url = ... })` moves the entry
/// cleanly onto `openai-responses` with zero duplicate problems, and a third-party plugin
/// declaring `providers = { "bedrock" }` is not hijacked as a Chat Completions endpoint.
#[tokio::test]
async fn wire_api_overlay_moves_the_entry_once() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("DEEPSEEK_API_KEY", "test-ds");
        let server = MockServer::start().await;
        let base = &server.base;
        server.route("/ds-resp/responses", vec![Resp::sse(responses_text("ds-via-responses"))]);

        write_user_plugin(
            &env,
            "third-party-bedrock",
            r#"return {
                name = "third-party-bedrock",
                inject = { "llm" },
                providers = { "bedrock" },
                apply = function(ctx)
                    ctx.llm:register_provider({
                        provider = "bedrock",
                        is_configured = function() return true end,
                        stream = function()
                            return { role = "assistant", content = { { type = "text", text = "from-bedrock" } }, stop_reason = "stop", usage = {} }
                        end,
                        models = { { model_id = "claude", context_window = 64000, max_output = 4096 } },
                    })
                end,
            }"#,
        );

        let h = env
            .host()
            .user_plugins()
            .config(json!({
                "providers": {
                    "deepseek": {
                        "wire_api": "responses",
                        "base_url": format!("{base}/ds-resp")
                    },
                    "bedrock": {
                        "base_url": format!("{base}/bedrock-unused")
                    }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (route, compat_probs, openai_probs): (String, usize, usize) = eval(
            &h,
            r#"
            local m = assert(LLM:resolve("deepseek/deepseek-chat"))
            local r = LLM:route_of(m)
            return tostring(r),
                #mare.plugins.status("provider-openai-compat").problems,
                #mare.plugins.status("provider-openai").problems
            "#,
        )
        .await;
        assert_eq!(route, "responses", "deepseek must have moved to responses route");
        assert_eq!(compat_probs, 0, "provider-openai-compat must have no duplicate/undeclared problems");
        assert_eq!(openai_probs, 0, "provider-openai must have no duplicate/undeclared problems");

        let msg = ask(&h, "deepseek/deepseek-chat").await;
        assert_eq!(text_of(&msg), "ds-via-responses");
        assert_eq!(server.count("/ds-resp/responses"), 1);

        let bedrock_msg = ask(&h, "bedrock/claude").await;
        assert_eq!(text_of(&bedrock_msg), "from-bedrock");

        h.shutdown().await;
    })
    .await
}

/// `/reload config` changing provider config restarts `llm` via `Fiber:update` without emitting
/// `kernel/reloaded`; persisting the post-startup ready flag in `mare.g` keeps `current()`'s
/// configured-provider fallback and background disk hydration working after config reload.
#[tokio::test]
async fn reload_config_changing_provider_preserves_startup_done_for_current_and_hydration() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("DEEPSEEK_API_KEY", "test-deepseek");
        let cache_dir = env.home.path().join("state/models-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            cache_dir.join("deepseek.json"),
            r#"{"fetched_at": 9999999999, "models": [{"id": "cached-after-cfg-reload", "context_window": 61000}]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = |port: u16, retries: u8| {
            format!(
                r#"mare.o.model = ""
mare.plugin("llm", {{ retries = {retries} }})
mare.provider("deepseek", {{ base_url = "http://127.0.0.1:{port}/v1" }})
"#
            )
        };
        std::fs::write(env.config_dir().join("init.lua"), init(18001, 4)).unwrap();

        let h = env.host().user_config(false).no_session().build().await;

        std::fs::write(env.config_dir().join("init.lua"), init(18002, 5)).unwrap();
        let report = h.reload_config().await.unwrap();
        assert!(
            report["reloaded"].as_array().unwrap().iter().any(|n| n == "llm"),
            "changing mare.provider(...) must restart llm via reload_config: {report}"
        );
        global(&h, "LLM", h.service_table("llm").unwrap());

        let cur_provider: String = eval(
            &h,
            r#"
            local c = LLM:current()
            local _ = LLM:models()
            return c and tostring(c.provider) or ""
            "#,
        )
        .await;
        assert_eq!(
            cur_provider, "deepseek",
            "after reload_config, current() must still select the configured provider"
        );

        wait_until(WAIT_SECS, "background catalog hydration after reload_config", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"
                    local m = LLM:resolve("deepseek/cached-after-cfg-reload")
                    return m ~= nil and m.listed == true and m.context_window == 61000
                    "#,
                )
                .await
            }
        })
        .await;

        h.shutdown().await;
    })
    .await
}

/// `readiness.prepare` (`LLM:prepare_request`) must preserve the caller's `args.model` when no
/// hydration occurred, and must replace `args.model` when a concurrent `refresh_models` updates
/// the provider's catalog while `ensure_loaded` is waiting on the per-provider lock (monotonic
/// `hydrated_gen` bumped by both hydration and `refresh_models`).
#[tokio::test]
async fn readiness_prepare_preserves_model_when_already_hydrated_and_replaces_when_refresh_hydrates() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = &server.base;
        server.route(
            "/refprov/models",
            vec![Resp::status(200, r#"{"data":[{"id":"seed-p","context_window":88000}]}"#)],
        );

        let h = env.host().no_session().build_exposed().await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (kept_tag, kept_max, refreshed_ctx): (String, i64, i64) = eval(
            &h,
            &format!(
                r#"
                -- 1. When provider has no pending hydration, prepare_request must keep caller's args.model intact.
                local rm1 = LLM:register_provider({{
                  provider = "keep-prep",
                  replace = true,
                  is_configured = function() return true end,
                  stream = function() return {{ role = "assistant", content = {{}}, stop_reason = "stop", usage = {{}} }} end,
                  models = {{ {{ model_id = "m1", max_output = 8192 }} }},
                }})
                local base_m = assert(LLM:resolve("keep-prep/m1"))
                local custom_m = table.copy(base_m)
                custom_m.max_output = 2345
                custom_m._caller_tag = "keep-me"
                local args1 = {{ model = custom_m }}
                LLM:prepare_request(args1)
                local kept_tag = tostring(args1.model and args1.model._caller_tag or "")
                local kept_max = tonumber(args1.model and args1.model.max_output or 0)
                rm1()

                -- 2. When refresh_models holds the catalog lock and updates models while prepare_request
                --    waits in ensure_loaded, prepare_request must observe the monotonic hydrated_gen bump
                --    and replace args.model with the refreshed capabilities.
                local rm2 = LLM:register_provider({{
                  provider = "refprov",
                  replace = true,
                  is_configured = function() return true end,
                  stream = function() return {{ role = "assistant", content = {{}}, stop_reason = "stop", usage = {{}} }} end,
                  catalog_scope = function() return "ref-scope" end,
                  catalog = function()
                    return {{
                      url = "{base}/refprov/models",
                      no_key = true,
                      decode = function(raw)
                        local j = mare.json.decode(raw)
                        return j.data
                      end,
                    }}
                  end,
                  models = {{ {{ model_id = "seed-p", context_window = 16000, max_output = 4096 }} }},
                }})

                local orig_write = mare.fs.write
                local write_entered_tx, write_entered_rx = mare.task.channel()
                local write_release_tx, write_release_rx = mare.task.channel()
                mare.fs.write = function(p, ...)
                  if type(p) == "string" and p:find("models-cache/refprov.json", 1, true) then
                    write_entered_tx:send(true)
                    write_release_rx:recv()
                  end
                  return orig_write(p, ...)
                end

                local ref_done_tx, ref_done_rx = mare.task.channel()
                mare.task.spawn(function()
                  LLM:refresh_models("refprov")
                  ref_done_tx:send(true)
                end)

                local function recv_or_timeout(rx, label)
                  local _, v = mare.task.select(function()
                    return rx:recv()
                  end, function()
                    mare.task.sleep(3000)
                    error("timeout waiting for " .. label, 0)
                  end)
                  return v
                end

                recv_or_timeout(write_entered_rx, "write_entered")

                -- Capture static seed while refresh_models holds the per-provider lock (pending_catalog is still true).
                local seed_m = assert(LLM:resolve("refprov/seed-p"))
                assert(seed_m.context_window == 16000)

                local prep_done_tx, prep_done_rx = mare.task.channel()
                local args2 = {{ model = seed_m }}
                mare.task.spawn(function()
                  LLM:prepare_request(args2)
                  prep_done_tx:send(tonumber(args2.model and args2.model.context_window or 0))
                end)
                mare.task.sleep(30)
                write_release_tx:send(true)

                recv_or_timeout(ref_done_rx, "ref_done")
                local refreshed_ctx = recv_or_timeout(prep_done_rx, "prep_done")
                mare.fs.write = orig_write
                rm2()
                return kept_tag, kept_max, refreshed_ctx
                "#
            ),
        )
        .await;

        assert_eq!(kept_tag, "keep-me", "prepare_request must preserve caller model when no hydration occurred");
        assert_eq!(kept_max, 2345, "prepare_request must preserve caller max_output when no hydration occurred");
        assert_eq!(
            refreshed_ctx, 88000,
            "prepare_request waiting behind refresh_models must observe hydrated_gen bump and replace args.model"
        );

        h.shutdown().await;
    })
    .await
}

/// Ordinary user `mare.provider(...)` config entries
/// (both default chat and `wire_api = "responses"`, plus preset overlay and cache capability flags)
/// must produce ZERO `kernel/doctor` or plugin declaration problems; an unknown field overlaid onto
/// a shipped preset must be reported via `kernel/doctor` without blaming `provider-openai-compat`.
#[tokio::test]
async fn ordinary_user_provider_config_has_zero_doctor_problems() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = &server.base;
        let h = env
            .host()
            .no_session()
            .config(json!({
                "providers": {
                    "deepseek": { "base_url": format!("{base}/ds") },
                    "mychat": {
                        "base_url": format!("{base}/mychat"),
                        "api_key": "k-chat",
                        "models": [{ "model_id": "m1", "name": "My Chat" }]
                    },
                    "myresp": {
                        "wire_api": "responses",
                        "base_url": format!("{base}/myresp"),
                        "api_key": "k-resp",
                        "supports_explicit_prompt_cache_mode": true,
                        "supports_in_memory_cache_retention": true,
                        "models": [{ "model_id": "r1", "name": "My Resp" }]
                    }
                }
            }))
            .build_exposed()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (doctor_fails, dir_probs, compat_probs, openai_probs, chat_warm, resp_warm): (
            Vec<String>,
            usize,
            usize,
            usize,
            String,
            String,
        ) = eval(
            &h,
            r#"
            local checks = {}
            C.cordis:get("bus"):emit("kernel/doctor", checks)
            local fails = {}
            for _, c in ipairs(checks) do
              if c.ok == false and type(c.name) == "string" and c.name:find("^provider directory") then
                fails[#fails + 1] = c.name .. ":" .. tostring(c.detail or "")
              end
            end
            local compat_p = #(mare.plugins.status("provider-openai-compat").problems or {})
            local openai_p = #(mare.plugins.status("provider-openai").problems or {})
            local llm_p = #(mare.plugins.status("llm").problems or {})
            local pc = assert(LLM:provider("mychat"))
            local pr = assert(LLM:provider("myresp"))
            return fails, llm_p, compat_p, openai_p, tostring(pc.warm_url and pc.warm_url() or ""), tostring(pr.warm_url and pr.warm_url() or "")
            "#,
        )
        .await;
        assert!(
            doctor_fails.is_empty(),
            "kernel/doctor must report 0 provider directory failures for ordinary user config: {doctor_fails:?}"
        );
        assert_eq!(dir_probs, 0, "llm must report 0 declaration problems for ordinary user provider config");
        assert_eq!(compat_probs, 0, "provider-openai-compat must report 0 problems");
        assert_eq!(openai_probs, 0, "provider-openai must report 0 problems");
        assert_eq!(chat_warm, format!("{base}/mychat"), "openai-chat factory must derive warm_url from base_url");
        assert_eq!(resp_warm, format!("{base}/myresp"), "openai-responses factory must derive warm_url from base_url");

        h.shutdown().await;
        drop(env);

        // User typo overlaid onto a shipped preset must report via kernel/doctor, not on provider-openai-compat.
        let env2 = TestEnv::new().await;
        let h2 = env2
            .host()
            .no_session()
            .config(json!({
                "providers": {
                    "deepseek": { "typo_on_preset": true }
                }
            }))
            .build_exposed()
            .await;
        let (preset_doctor_fails, preset_compat_probs): (Vec<String>, usize) = eval(
            &h2,
            r#"
            local checks = {}
            C.cordis:get("bus"):emit("kernel/doctor", checks)
            local fails = {}
            for _, c in ipairs(checks) do
              if c.ok == false and type(c.name) == "string" and c.name:find("^provider directory") then
                fails[#fails + 1] = c.name .. ":" .. tostring(c.detail or "")
              end
            end
            return fails, #(mare.plugins.status("provider-openai-compat").problems or {})
            "#,
        )
        .await;
        assert!(
            preset_doctor_fails.iter().any(|s| s.contains("deepseek") && s.contains("typo_on_preset")),
            "kernel/doctor must report user typo overlaid on shipped preset: {preset_doctor_fails:?}"
        );
        assert_eq!(
            preset_compat_probs, 0,
            "user overlay typo on shipped preset must NOT blame provider-openai-compat"
        );
        h2.shutdown().await;
    })
    .await
}

/// DeepSeek placeholder reasoning must match Fireworks (`accounts/fireworks/models/deepseek-r1`),
/// Bedrock (`us.deepseek.r1-v1:0`), and gateway URLs containing `deepseek` across both Chat
/// Completions and Responses wires.
#[tokio::test]
async fn deepseek_placeholder_reasoning_matches_fireworks_and_bedrock_ids() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = &server.base;
        let ok_chat = compat_events(&[compat_text("ok"), compat_stop()]);
        for path in [
            "/fw-chat/chat/completions",
            "/br-chat/chat/completions",
            "/deepseek-gw/chat/completions",
        ] {
            server.route(path, vec![Resp::sse(ok_chat.clone())]);
        }
        for path in ["/fw-resp/responses", "/br-resp/responses"] {
            server.route(path, vec![Resp::sse(responses_text("ok"))]);
        }

        let h = env
            .host()
            .no_session()
            .config(json!({
                "providers": {
                    "fw-chat": {
                        "base_url": format!("{base}/fw-chat"),
                        "no_key": true,
                        "models": [{ "model_id": "accounts/fireworks/models/deepseek-r1" }]
                    },
                    "br-chat": {
                        "base_url": format!("{base}/br-chat"),
                        "no_key": true,
                        "models": [{ "model_id": "us.deepseek.r1-v1:0" }]
                    },
                    "gw-chat": {
                        "base_url": format!("{base}/deepseek-gw"),
                        "no_key": true,
                        "models": [{ "model_id": "custom-model" }]
                    },
                    "fw-resp": {
                        "wire_api": "responses",
                        "base_url": format!("{base}/fw-resp"),
                        "no_key": true,
                        "models": [{ "model_id": "accounts/fireworks/models/deepseek-r1" }]
                    },
                    "br-resp": {
                        "wire_api": "responses",
                        "base_url": format!("{base}/br-resp"),
                        "no_key": true,
                        "models": [{ "model_id": "us.deepseek.r1-v1:0" }]
                    }
                }
            }))
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        for model_ref in [
            "fw-chat/accounts/fireworks/models/deepseek-r1",
            "br-chat/us.deepseek.r1-v1:0",
            "gw-chat/custom-model",
            "fw-resp/accounts/fireworks/models/deepseek-r1",
            "br-resp/us.deepseek.r1-v1:0",
        ] {
            let ok: bool = h
                .lua()
                .load(
                    r#"
                    local ref = ...
                    local m = assert(LLM:resolve(ref), "missing " .. ref)
                    local msg = LLM:stream({
                      model = m,
                      tools = { { name = "ping", description = "ping", parameters = { type = "object", properties = {} } } },
                      messages = {
                        { role = "user", content = { { type = "text", text = "hi" } } },
                        { role = "assistant", content = { { type = "tool_call", id = "c1", name = "ping", arguments = {} } } },
                        { role = "tool", tool_call_id = "c1", content = { { type = "text", text = "pong" } } },
                      },
                    }, function() end)
                    assert(msg.stop_reason ~= "error", ref .. ": " .. tostring(msg.error))
                    return true
                    "#,
                )
                .into_function()
                .unwrap()
                .call_async::<bool>(model_ref.to_string())
                .await
                .unwrap();
            assert!(ok);
        }

        let req_by_path = |p: &str| {
            server
                .requests()
                .into_iter()
                .find(|r| r.path.starts_with(p))
                .unwrap_or_else(|| panic!("missing request for {p}"))
                .json()
        };

        for (label, path) in [
            ("fireworks chat", "/fw-chat/"),
            ("bedrock chat", "/br-chat/"),
            ("deepseek url chat", "/deepseek-gw/"),
        ] {
            let body = req_by_path(path);
            let assistant = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "assistant")
                .unwrap();
            assert!(
                assistant.get("reasoning_content").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()),
                "{label} must inject placeholder reasoning_content on assistant tool-call turn: {assistant}"
            );
        }

        for (label, path) in [("fireworks responses", "/fw-resp/"), ("bedrock responses", "/br-resp/")] {
            let body = req_by_path(path);
            let has_reasoning = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "reasoning");
            assert!(
                has_reasoning,
                "{label} must inject placeholder reasoning item on assistant tool-call turn: {}",
                body["input"]
            );
        }

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn register_providers_batches_registration_and_coalesces_changed_event() {
    local(async {
        let env = TestEnv::new().await;
        write_user_plugin(
            &env,
            "third-party-batch",
            r#"
return {
    name = "third-party-batch",
    inject = { "llm", "bus" },
    provide = { "third-party-batch-ctrl" },
    providers = { "batch-a", "batch-b", "batch-c" },
    contracts = {
        ["third-party-batch-ctrl"] = {
            methods = {
                dispose = { doc = "dispose registered batch", expose = "lua", result = { type = "boolean" } },
            },
        },
    },
    apply = function(ctx)
        local events = 0
        ctx:on("llm/providers-changed", function()
            events = events + 1
            mare.g.batch_events = events
        end)
        local dispose, errs = ctx.llm:register_providers({
            {
                provider = "batch-a",
                wire = "openai-chat",
                base_url = "https://a.example/v1",
                no_key = true,
                models = { { model_id = "m-a", name = "Model A" } },
            },
            {
                provider = "batch-b",
                wire = "openai-chat",
                base_url = "https://b.example/v1",
                no_key = true,
                models = { { model_id = "m-b", name = "Model B" } },
            },
            {
                provider = "batch-c",
                wire = "openai-chat",
                base_url = "https://c.example/v1",
                no_key = true,
                models = { { model_id = "m-c", name = "Model C" } },
            },
        })
        mare.g.batch_register_events = events
        mare.g.batch_register_err_count = errs and #errs or 0
        require("mare_lib").service(ctx, "third-party-batch-ctrl", {
            dispose = function()
                return dispose()
            end,
        })
    end,
}
"#,
        );
        let h = env.host().user_plugins().no_session().build_exposed().await;
        let (
            reg_events,
            err_count,
            has_all,
            compat_ok,
            dispose_events,
            second_dispose_events,
            after_gone,
            mixed_events,
            mixed_errs,
            mixed_valid_ok,
            mixed_dispose_events,
        ): (i64, i64, bool, bool, i64, i64, bool, i64, i64, bool, i64) = eval(
            &h,
            r#"
            local llm = C.cordis:get("llm")
            local ctrl = C.cordis:get("third-party-batch-ctrl")
            local has_all = llm:provider("batch-a") ~= nil
                and llm:provider("batch-b") ~= nil
                and llm:provider("batch-c") ~= nil
            local compat_ok = true
            for _, p in ipairs({ "deepseek", "kimi", "kimi-code", "openrouter", "ollama", "lmstudio" }) do
                if llm:provider(p) == nil then
                    compat_ok = false
                end
            end
            local before = mare.g.batch_events or 0
            assert(ctrl:dispose() == true)
            local after_first = mare.g.batch_events or 0
            assert(ctrl:dispose() == false)
            local after_second = mare.g.batch_events or 0
            local after_gone = llm:provider("batch-a") == nil
                and llm:provider("batch-b") == nil
                and llm:provider("batch-c") == nil

            -- Mixed valid + invalid batch (both data-tier and code-tier failures) reports each
            -- invalid entry while still registering valid entries with a single notification.
            local m_events = 0
            local off = C.cordis:on("llm/providers-changed", function()
                m_events = m_events + 1
            end)
            local rm_mixed, m_errs = llm:register_providers({
                {
                    provider = "mixed-ok-1",
                    wire = "openai-chat",
                    base_url = "https://m1.example/v1",
                    no_key = true,
                    models = { { model_id = "m1", name = "M1" } },
                },
                {
                    provider = "",
                    wire = "openai-chat",
                    base_url = "https://bad.example/v1",
                },
                {
                    provider = "bad-code",
                    stream = "not-a-function",
                    models = {},
                },
                {
                    provider = "mixed-ok-2",
                    wire = "openai-chat",
                    base_url = "https://m2.example/v1",
                    no_key = true,
                    models = { { model_id = "m2", name = "M2" } },
                },
            })
            local mixed_events = m_events
            local mixed_valid_ok = llm:provider("mixed-ok-1") ~= nil
                and llm:provider("mixed-ok-2") ~= nil
                and llm:provider("bad-code") == nil
            assert(m_errs and #m_errs == 2 and m_errs[1].index == 2 and m_errs[2].index == 3 and m_errs[2].provider == "bad-code")
            m_events = 0
            assert(rm_mixed() == true)
            local mixed_dispose_events = m_events
            assert(llm:provider("mixed-ok-1") == nil and llm:provider("mixed-ok-2") == nil)

            -- Plugin unload (fiber dispose) releases all batch-registered providers and emits once.
            local pack_fiber = C.cordis:plugin({ name = "batch-unload-plugin", apply = function() end })
            m_events = 0
            pack_fiber.ctx:get("llm"):register_providers({
                {
                    provider = "unload-a",
                    wire = "openai-chat",
                    base_url = "https://ua.example/v1",
                    no_key = true,
                    models = { { model_id = "ua" } },
                },
                {
                    provider = "unload-b",
                    wire = "openai-chat",
                    base_url = "https://ub.example/v1",
                    no_key = true,
                    models = { { model_id = "ub" } },
                },
            })
            assert(m_events == 1, "batch register on plugin fiber must emit once")
            assert(llm:provider("unload-a") ~= nil and llm:provider("unload-b") ~= nil)
            m_events = 0
            local d = pack_fiber:dispose()
            if type(d) == "table" and d.await then d:await() end
            assert(m_events == 1, "unloading plugin fiber must emit llm/providers-changed once, got " .. m_events)
            assert(llm:provider("unload-a") == nil and llm:provider("unload-b") == nil)

            off()
            return mare.g.batch_register_events,
                mare.g.batch_register_err_count,
                has_all,
                compat_ok,
                after_first - before,
                after_second - after_first,
                after_gone,
                mixed_events,
                m_errs and #m_errs or 0,
                mixed_valid_ok,
                mixed_dispose_events
            "#,
        )
        .await;
        assert_eq!(reg_events, 1, "batch register_providers must emit llm/providers-changed exactly once");
        assert_eq!(err_count, 0, "valid third-party batch must return nil errors");
        assert!(has_all, "all valid providers in batch must be registered");
        assert!(compat_ok, "all 6 provider-openai-compat presets must be registered");
        assert_eq!(dispose_events, 1, "batch dispose must emit llm/providers-changed exactly once");
        assert_eq!(second_dispose_events, 0, "second dispose must be idempotent and emit 0 events");
        assert!(after_gone, "batch dispose must remove every registered provider");
        assert_eq!(mixed_events, 1, "mixed batch must emit llm/providers-changed exactly once");
        assert_eq!(mixed_errs, 2, "invalid data-tier and code-tier entries must both be reported in returned errors");
        assert!(mixed_valid_ok, "valid entries in mixed batch must still register");
        assert_eq!(mixed_dispose_events, 1, "mixed batch dispose must emit llm/providers-changed once");
        h.shutdown().await;
    })
    .await
}
