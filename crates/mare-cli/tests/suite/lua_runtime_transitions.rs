//! Regression tests for Lua plugin runtime transitions: command aliases, invocation-only config
//! options, compaction backoff/watermark resets, branch summaries, web/context invalidation,
//! terminal quota/OSC-133 wakeups, and statusline quota/gauge rendering.

use crate::common::*;
use serde_json::json;

/// `mare.command.alias` in `init.lua` works even when the target command (`resume`) is registered
/// by a plugin loaded after `user-config`, and `/config` refuses to edit `invocation_only` options
/// (`effort`) into `state.json` where `llm/params.lua` deliberately ignores them.
#[tokio::test]
async fn user_config_alias_resolves_later_commands_and_config_guards_invocation_only_effort() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), r#"mare.command.alias("r", "resume")"#).unwrap();

        let mut b = env
            .host()
            .builtins(&["user-config", "commands", "session"])
            .faux(json!([]))
            .config(json!({ "logo": false }));
        b.opts.load_init = true;
        let h = b.build().await;
        expose(&h);

        let ok: bool = eval(
            &h,
            r#"
            local cmds = C.cordis:get("commands")
            local target = cmds:get("resume")
            assert(target ~= nil, "expected session plugin to register /resume")
            local alias = cmds:get("r")
            assert(alias ~= nil, "alias /r targeting later-loaded /resume must not be dropped")
            assert(alias.alias_of == "resume", "unexpected alias_of: " .. tostring(alias.alias_of))

            -- /config must treat invocation_only options (like effort) as non-editable so they
            -- cannot be saved into state.json where llm/params.lua ignores them.
            assert(mare.options.effort and mare.options.effort.invocation_only == true)
            local orig_ui = _G.ui
            local orig_select = orig_ui.select
            local orig_input = orig_ui.input
            local orig_connected = orig_ui.connected
            local orig_notify = orig_ui.notify
            local input_called = false
            local effort_preview_editable = nil
            local _restore <close> = setmetatable({}, {
                __close = function()
                    orig_ui.select = orig_select
                    orig_ui.input = orig_input
                    orig_ui.connected = orig_connected
                    orig_ui.notify = orig_notify
                end,
            })
            orig_ui.connected = function() return true end
            orig_ui.notify = function() end
            orig_ui.input = function()
                input_called = true
                return "high"
            end
            orig_ui.select = function(opts)
                if opts and opts.title == "Configuration" and opts.tabs then
                    local items = opts.tabs[1].items
                    for idx, it in ipairs(items) do
                        if tostring(it.label):match("^effort%s*$") then
                            local encoded = mare.json.encode(it.preview or {})
                            effort_preview_editable = encoded:find("Enter: change", 1, true) ~= nil
                            return idx, 1
                        end
                    end
                end
                return nil
            end
            local cfg_fn = require("commands.config")
            cfg_fn(C.cordis, "")
            assert(effort_preview_editable == false, "effort preview must not advertise Enter: change")
            assert(input_called == false, "selecting invocation_only effort in /config must not open editor")
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await;
}

/// `compact` clears failure backoff on `llm/login` and `llm/providers-changed`, and resets
/// `warned_near` when `session/loaded` or model switches invoke `reset_compaction_guard`.
#[tokio::test]
async fn compact_clears_backoff_on_login_and_resets_warned_near_on_session_load() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["compact", "usage", "session"])
            .faux(json!([]))
            .config(json!({
                "logo": false,
                "plugins": {
                    "compact": {
                        "auto": true,
                        "stages": ["fail_probe"],
                        "threshold_tokens": 1000,
                        "headroom_tokens": 0,
                        "failure_backoff_ms": 60000,
                        "failure_backoff_max_ms": 600000
                    }
                }
            }))
            .build()
            .await;
        expose(&h);

        let ok: bool = eval(
            &h,
            r#"
            local approaching = 0
            C.cordis:on("compact/approaching", function()
                approaching = approaching + 1
            end)

            local model = C.cordis:get("llm"):current()
            local cmp = C.cordis:get("compact")
            local small_msg = { role = "user", content = { { type = "text", text = string.rep("x", 3650) } } }
            A:set_messages({ small_msg })

            -- 1. Reach > 90% of threshold (1000 tokens) -> compact/approaching fires once.
            cmp:prepare_request({
                model = model,
                system = "",
                tools = {},
                messages = { small_msg },
            }, { small_msg })
            assert(approaching == 1, "expected first compact/approaching, got " .. tostring(approaching))

            -- 2. Loading another session/branch must reset warned_near so approaching fires again.
            C.cordis:emit("session/loaded", { messages = { small_msg }, navigated = true })
            cmp:prepare_request({
                model = model,
                system = "",
                tools = {},
                messages = { small_msg },
            }, { small_msg })
            assert(approaching == 2, "warned_near must reset on session/loaded; approaching=" .. tostring(approaching))

            -- 3. Trigger a compaction failure to enter 60s failure_backoff, then emit llm/login.
            local big_msg = { role = "user", content = { { type = "text", text = string.rep("z", 6000) } } }
            A:set_messages({ big_msg })
            local disp = cmp:register_stage("fail_probe", function()
                error("simulated auth 401 failure")
            end)
            local _cleanup <close> = setmetatable({}, { __close = function() disp() end })

            local started = 0
            C.cordis:on("compact/started", function()
                started = started + 1
            end)
            cmp:prepare_request({
                model = model,
                system = "",
                tools = {},
                messages = { big_msg },
            }, { big_msg })
            assert(started == 1, "expected first auto compaction attempt, got " .. tostring(started))

            -- Immediate retry while still in 60s backoff must be skipped.
            cmp:prepare_request({
                model = model,
                system = "",
                tools = {},
                messages = { big_msg },
            }, { big_msg })
            assert(started == 1, "expected second attempt to be skipped during backoff, got " .. tostring(started))

            -- After llm/login, backoff clears immediately so next prepare retries compaction.
            C.cordis:emit("llm/login", { provider = model.provider })
            cmp:prepare_request({
                model = model,
                system = "",
                tools = {},
                messages = { big_msg },
            }, { big_msg })
            assert(started == 2, "llm/login must clear compaction failure backoff; started=" .. tostring(started))

            -- That retry failed too, re-entering 60s backoff; immediate retry must be skipped.
            cmp:prepare_request({
                model = model,
                system = "",
                tools = {},
                messages = { big_msg },
            }, { big_msg })
            assert(started == 2, "expected retry to be skipped during backoff after login retry, got " .. tostring(started))

            -- After llm/providers-changed, backoff clears immediately as well.
            C.cordis:emit("llm/providers-changed", {})
            cmp:prepare_request({
                model = model,
                system = "",
                tools = {},
                messages = { big_msg },
            }, { big_msg })
            assert(started == 3, "llm/providers-changed must clear compaction failure backoff; started=" .. tostring(started))
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
        drop(env);

        // 4. Verify `S.stalled` circuit-breaker clears on both `llm/providers-changed` and `llm/login`.
        // Use `failure_backoff_ms = 0` so only `S.stalled` gates retries (no timer dependency).
        let env2 = TestEnv::new().await;
        let h2 = env2
            .host()
            .builtins(&["compact", "usage", "session"])
            .faux(json!([]))
            .config(json!({
                "logo": false,
                "plugins": {
                    "compact": {
                        "auto": true,
                        "stages": ["local"],
                        "threshold_tokens": 1000,
                        "headroom_tokens": 0,
                        "keep_recent_tokens": 0,
                        "failure_backoff_ms": 0,
                        "failure_backoff_max_ms": 0
                    }
                }
            }))
            .build()
            .await;
        expose(&h2);
        let ok2: bool = eval(
            &h2,
            r#"
            local model = C.cordis:get("llm"):current()
            local cmp = C.cordis:get("compact")
            local m1 = { role = "user", content = { { type = "text", text = string.rep("a", 800) } } }
            local m2 = { role = "assistant", content = { { type = "text", text = string.rep("b", 800) } } }
            local m3 = { role = "user", content = { { type = "text", text = "latest" } } }
            A:set_messages({ m1, m2, m3 })

            local started, stalled = 0, 0
            C.cordis:on("compact/started", function() started = started + 1 end)
            C.cordis:on("compact/completed", function(ev)
                if type(ev) == "table" and ev.stalled == true then
                    stalled = stalled + 1
                end
            end)

            -- Fixed system overhead > 90% of threshold forces local_summary exhaustion -> S.stalled = true.
            local huge_system = string.rep("s", 5000)
            local function probe()
                cmp:prepare_request({
                    model = model,
                    system = huge_system,
                    tools = {},
                    messages = { m1, m2, m3 },
                }, { m1, m2, m3 })
            end

            probe()
            assert(started == 1 and stalled == 1, "expected exhaustion to open stalled circuit; started=" .. tostring(started) .. " stalled=" .. tostring(stalled))

            -- While stalled, prepare_request must skip auto compaction even with 0ms backoff.
            probe()
            assert(started == 1, "stalled circuit must suppress auto compaction; started=" .. tostring(started))

            -- llm/providers-changed clears S.stalled.
            C.cordis:emit("llm/providers-changed", {})
            probe()
            assert(started == 2 and stalled == 2, "llm/providers-changed must clear stalled circuit; started=" .. tostring(started) .. " stalled=" .. tostring(stalled))

            probe()
            assert(started == 2, "stalled circuit must re-open after exhaustion; started=" .. tostring(started))

            -- llm/login also clears S.stalled.
            C.cordis:emit("llm/login", { provider = model.provider })
            probe()
            assert(started == 3 and stalled == 3, "llm/login must clear stalled circuit; started=" .. tostring(started) .. " stalled=" .. tostring(stalled))
            return true
            "#,
        )
        .await;
        assert!(ok2);
        h2.shutdown().await;
    })
    .await;
}

/// `mare_lib.path_usage_stale` treats `branch_summary` like `compaction`, and creating a branch
/// summary notifies `usage`/`compact` so context tokens reflect the new branch summary.
#[tokio::test]
async fn branch_summary_marks_usage_stale_and_updates_context_tokens() {
    local(async {
        let env = TestEnv::new().await;
        env.write_session(
            "2026-01-01T00-00-00_branch_usage.jsonl",
            &[
                msg("u1", None, "user", "start"),
                msg("a1", Some("u1"), "assistant", "ok"),
                msg("u2", Some("a1"), "user", "explore branch with detailed context"),
                msg("a2", Some("u2"), "assistant", "branch exploration findings"),
            ],
        );
        let h = env
            .host()
            .builtins(&["session", "usage", "compact"])
            .faux(json!([{ "text": "Summary of the explored branch with detailed notes and findings." }]))
            .resume()
            .config(json!({ "logo": false }))
            .build()
            .await;
        expose(&h);

        let ok: bool = eval(
            &h,
            r#"
            local lib = require("mare_lib")
            local path = {
                { type = "message", entry_id = "e1", message = { role = "user", content = { { type = "text", text = "hi" } } } },
                { type = "message", entry_id = "e2", message = { role = "assistant", content = { { type = "text", text = "ok" } }, usage = { input = 10, output = 5 } } },
                { type = "branch_summary", entry_id = "e3", parent_entry_id = "e2", summary = "Summary text" },
            }
            assert(lib.path_usage_stale(path, path, false) == true, "path_usage_stale must return true when branch_summary follows last assistant usage")

            -- branch_summary does not drop prior context, so pruning an entry before branch_summary
            -- after a later assistant reply must still mark provider usage stale.
            local path_with_prune = {
                { type = "message", entry_id = "u1", message = { role = "user", content = { { type = "text", text = "hi" } } } },
                { type = "message", entry_id = "tr1", message = { role = "tool_result", content = { { type = "text", text = "big tool output" } } } },
                { type = "branch_summary", entry_id = "bs1", parent_entry_id = "tr1", summary = "Earlier branch" },
                { type = "message", entry_id = "a2", message = { role = "assistant", content = { { type = "text", text = "done" } }, usage = { input = 20, output = 5 } } },
            }
            local entries_with_prune = {
                path_with_prune[1],
                path_with_prune[2],
                path_with_prune[3],
                path_with_prune[4],
                { type = "prune", target_entry_ids = { tr1 = true } },
            }
            assert(
                lib.path_usage_stale(path_with_prune, entries_with_prune, false) == true,
                "branch_summary must not shift prune scan start past earlier active entries"
            )

            -- End-to-end: rewind with summarize=true summarizes before moving leaf and emits session/loaded once.
            local usage = C.cordis:get("usage")
            local loaded_events = 0
            C.cordis:on("session/loaded", function()
                loaded_events = loaded_events + 1
            end)
            usage:set_context(0)
            local r = S:rewind({ session_id = S:info().session_id, before_item_id = "a1", summarize = true })
            assert(type(r) == "table", "rewind failed: " .. tostring(r))
            assert(loaded_events == 1, "rewind with summarize=true must emit session/loaded exactly once; got " .. tostring(loaded_events))
            assert((usage:context_tokens() or 0) > 0, "usage:context_tokens() must reflect the new branch summary")
            local p = S:path_to()
            assert(p[#p] and p[#p].type == "branch_summary" and p[#p].parent_entry_id == "u1", "branch_summary must attach directly to target parent u1")
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await;
}

/// Changing `web` mode via `/web mode` or `mare.o.web_search` (even without `commands`) invalidates
/// both `tools` and `context`, and `web.registry` honors `cfg.search_timeout_ms` / `cfg.fetch_timeout_ms`.
#[tokio::test]
async fn web_mode_changes_invalidate_tools_context_and_honor_configured_timeout() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["web", "tools", "context", "commands"])
            .faux(json!([]))
            .config(json!({
                "logo": false,
                "plugins": {
                    "web": {
                        "web_search": "live",
                        "search_timeout_ms": 60,
                        "fetch_timeout_ms": 60
                    }
                }
            }))
            .build()
            .await;
        expose(&h);

        let ok: bool = eval(
            &h,
            r#"
            local web = C.cordis:get("web")
            local tools = C.cordis:get("tools")
            local context = C.cordis:get("context")
            local cmds = C.cordis:get("commands")

            web:register_search({
                provider_id = "mock-search",
                search = function(_req, call)
                    call.cancel:wait()
                    return { data = {} }
                end,
            })
            web:register_fetch({
                provider_id = "mock-fetch",
                fetch = function(_req, call)
                    call.cancel:wait()
                    return { content = "ok" }
                end,
            })

            -- Configured 60ms timeout must trigger WEB_TIMEOUT well before the 30s default.
            local ok_s, err_s
            local done_s = mare.task.timeout(400, function()
                ok_s, err_s = pcall(function() return web:search({ "rust" }) end)
            end)
            assert(done_s and not ok_s and tostring(err_s):find("WEB_TIMEOUT", 1, true), "expected search_timeout_ms=60 to fire WEB_TIMEOUT, done=" .. tostring(done_s) .. " err=" .. tostring(err_s))

            local ok_f, err_f
            local done_f = mare.task.timeout(400, function()
                ok_f, err_f = pcall(function() return web:fetch("https://example.com") end)
            end)
            assert(done_f and not ok_f and tostring(err_f):find("WEB_TIMEOUT", 1, true), "expected fetch_timeout_ms=60 to fire WEB_TIMEOUT, done=" .. tostring(done_f) .. " err=" .. tostring(err_f))

            -- Build context while live: must include web_search guidance.
            local sys_live = context:system_prompt()
            assert(sys_live:find("web_search results are external", 1, true), "expected web_search section when live")

            local before_rev = tools:revision()

            -- Switch to disabled via /web mode disabled: must invalidate both tools and context cache.
            cmds:run("/web mode disabled")
            local sys_disabled = context:system_prompt()
            assert(not sys_disabled:find("web_search results are external", 1, true), "disabling web_search must invalidate cached context prompt")
            assert(tools:revision() > before_rev, "/web mode disabled must bump tools:revision()")
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
        drop(env);

        // Also verify mare.o.web_search works when `commands` plugin is not loaded.
        let env2 = TestEnv::new().await;
        let h2 = env2
            .host()
            .builtins(&["web", "tools", "context"])
            .faux(json!([]))
            .config(json!({
                "logo": false,
                "plugins": {
                    "commands": { "enabled": false },
                    "web": { "web_search": "live" }
                }
            }))
            .build()
            .await;
        expose(&h2);
        let ok2: bool = eval(
            &h2,
            r#"
            assert(C.cordis:get("commands") == nil, "commands plugin must be disabled for this check")
            local web = C.cordis:get("web")
            local context = C.cordis:get("context")
            web:register_search({
                provider_id = "mock-search",
                search = function() return { data = {} } end,
            })
            assert(context:system_prompt():find("web_search results are external", 1, true))
            C.cordis:emit("option/changed", { name = "web_search", value = "disabled" })
            assert(not context:system_prompt():find("web_search results are external", 1, true), "option/changed must work without commands plugin")
            return true
            "#,
        )
        .await;
        assert!(ok2);
        h2.shutdown().await;
    })
    .await;
}

/// Subdirectory `AGENTS.md` deduplication and budget reset on branch navigation (`session/loaded`
/// with `navigated = true`, as emitted by `/undo` and `/tree`), so reading into the subdirectory on
/// a rewound branch re-injects its instructions.
#[tokio::test]
async fn subdir_agents_md_dedup_resets_on_branch_navigation() {
    local(async {
        let env = TestEnv::new().await;
        let sub = env.cwd.join("subpkg");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("AGENTS.md"), "SUBPKG_RULE: always use strict mode\n").unwrap();
        std::fs::write(sub.join("a.txt"), "file a\n").unwrap();
        std::fs::write(sub.join("b.txt"), "file b\n").unwrap();

        let h = env
            .host()
            .builtins(&["context"])
            .faux(json!([]))
            .config(json!({ "logo": false }))
            .build()
            .await;
        expose(&h);

        let ok: bool = eval(
            &h,
            r#"
            local function run_read(path)
                local res = C.cordis:get("bus"):waterfall("tool/after", {
                    call = { name = "read", arguments = { path = path } },
                    result = { content = "contents of " .. path, is_error = false },
                    operation = {},
                })
                return res.result.content
            end

            local first = run_read("subpkg/a.txt")
            assert(first:find("SUBPKG_RULE", 1, true), "expected first read to inject subpkg/AGENTS.md: " .. tostring(first))

            local dup = run_read("subpkg/b.txt")
            assert(not dup:find("SUBPKG_RULE", 1, true), "expected second read on same branch to deduplicate")

            -- Rewind/branch navigation emits session/loaded { navigated = true }.
            C.cordis:emit("session/loaded", { navigated = true, messages = {} })
            local after_undo = run_read("subpkg/b.txt")
            assert(after_undo:find("SUBPKG_RULE", 1, true), "expected subpkg/AGENTS.md to re-inject after branch navigation: " .. tostring(after_undo))
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await;
}

/// Subdirectory `AGENTS.md` deduplication and budget reset after a successful compaction so
/// compacted or pruned tool results do not permanently suppress directory instructions.
#[tokio::test]
async fn subdir_agents_md_dedup_resets_after_compaction() {
    local(async {
        let env = TestEnv::new().await;
        let sub = env.cwd.join("subpkg");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("AGENTS.md"), "SUBPKG_COMPACT_RULE: re-read after compaction\n").unwrap();
        std::fs::write(sub.join("a.txt"), "file a\n").unwrap();
        std::fs::write(sub.join("b.txt"), "file b\n").unwrap();

        let h = env.host().builtins(&["context"]).faux(json!([])).config(json!({ "logo": false })).build().await;
        expose(&h);

        let ok: bool = eval(
            &h,
            r#"
            local function run_read(path)
                local res = C.cordis:get("bus"):waterfall("tool/after", {
                    call = { name = "read", arguments = { path = path } },
                    result = { content = "contents of " .. path, is_error = false },
                    operation = {},
                })
                return res.result.content
            end

            local first = run_read("subpkg/a.txt")
            assert(first:find("SUBPKG_COMPACT_RULE", 1, true), "expected first read to inject subpkg/AGENTS.md")

            local dup = run_read("subpkg/b.txt")
            assert(not dup:find("SUBPKG_COMPACT_RULE", 1, true), "expected second read to deduplicate")

            -- A failed/no-op compaction must not reset deduplication.
            C.cordis:emit("compact/completed", { status = "failed", changed = false })
            local after_noop = run_read("subpkg/b.txt")
            assert(not after_noop:find("SUBPKG_COMPACT_RULE", 1, true), "no-op compaction must keep deduplication")

            -- A successful compaction resets included paths and budget so instructions re-inject.
            C.cordis:emit("compact/completed", { status = "completed", changed = true })
            local after_compact = run_read("subpkg/b.txt")
            assert(
                after_compact:find("SUBPKG_COMPACT_RULE", 1, true),
                "expected subpkg/AGENTS.md to re-inject after compaction: " .. tostring(after_compact)
            )
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await;
}

/// Naturally exited terminal sessions free both the per-owner active session quota and their
/// registered name, and silent commands that emit only OSC 133 prompt markers wake `terminal.send`
/// immediately without timing out.
#[cfg(unix)]
#[tokio::test]
async fn terminal_exited_sessions_free_quota_and_name_and_silent_osc133_wakes_send() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["terminal"])
            .faux(json!([]))
            .config(json!({ "logo": false }))
            .build()
            .await;
        expose(&h);

        let ok: bool = eval(
            &h,
            r#"
            local term = C.cordis:get("terminal")

            -- 1. Total sessions (running + exited) stay capped at 16; when full, the least-recently-used
            -- exited session is closed (snapshot retained in closed) while newer/recently-read ones stay.
            local ids = {}
            for i = 1, 16 do
                local s = term:create({
                    name = "short-lived",
                    cmd = "sh",
                    args = { "-c", "exit 0" },
                    emulate = false,
                })
                local r = term:read({ terminal_id = s.terminal_id, wait_ms = 5000 })
                assert(r.status == "completed", "expected short-lived #" .. i .. " to complete, got " .. tostring(r.status))
                ids[i] = s.terminal_id
            end
            assert(#term:list().data == 16, "expected 16 exited sessions before pruning, got " .. tostring(#term:list().data))

            -- Touch ids[1] so ids[2] becomes the least-recently-used exited session.
            term:read({ terminal_id = ids[1], output_offset = 0 })

            local s17 = term:create({
                name = "short-lived",
                cmd = "sh",
                args = { "-c", "exit 0" },
                emulate = false,
            })
            local r17 = term:read({ terminal_id = s17.terminal_id, wait_ms = 5000 })
            assert(r17.status == "completed", "expected 17th session to complete")

            local listed = term:list().data
            assert(#listed == 16, "total sessions must remain capped at 16 after pruning; got " .. tostring(#listed))
            local present = {}
            for _, row in ipairs(listed) do
                present[row.terminal_id] = true
            end
            assert(not present[ids[2]], "least-recently-used exited session ids[2] must be evicted")
            assert(present[ids[1]], "recently-read exited session ids[1] must be kept")
            assert(present[ids[16]] and present[s17.terminal_id], "newest exited sessions must be kept")
            local closed_snap = term:close({ terminal_id = ids[2] })
            assert(closed_snap and closed_snap.terminal_id == ids[2] and closed_snap.status == "completed", "evicted session snapshot must remain available via close()")

            -- Evict the remaining exited sessions by filling all 16 slots with running terminals;
            -- once all 16 are running, a 17th create must fail with terminal_limit_reached.
            local running = {}
            local _cleanup_running <close> = setmetatable({}, {
                __close = function()
                    for _, id in ipairs(running) do
                        pcall(term.close, term, { terminal_id = id })
                    end
                end,
            })
            for i = 1, 16 do
                local s = term:create({
                    cmd = "sh",
                    args = { "-c", "IFS= read -r _; exit 0" },
                    emulate = false,
                })
                running[i] = s.terminal_id
            end
            local ok_over, err_over = pcall(function()
                return term:create({ cmd = "sh", args = { "-c", "exit 0" }, emulate = false })
            end)
            assert(not ok_over and tostring(err_over):find("terminal session limit reached", 1, true), "17th running session must fail with terminal_limit_reached; got " .. tostring(err_over))
            for _, id in ipairs(running) do
                term:close({ terminal_id = id })
            end
            running = {}

            -- 2. Silent command emitting only OSC 133 prompt markers must wake terminal.send promptly.
            local s = term:create({
                cmd = "sh",
                args = {
                    "-c",
                    "stty -echo; printf 'READY\\n'; while IFS= read -r _; do printf '\\033]133;D;0\\007\\033]133;A\\007'; done",
                },
                emulate = true,
            })
            local _close <close> = setmetatable({}, {
                __close = function() pcall(term.close, term, { terminal_id = s.terminal_id }) end,
            })
            local ready = ""
            local off = 0
            for _ = 1, 20 do
                local r = term:read({ terminal_id = s.terminal_id, output_offset = off, wait_ms = 500 })
                ready = ready .. (r.text or "")
                off = r.next_output_offset or off
                if ready:find("READY", 1, true) then break end
            end
            assert(ready:find("READY", 1, true), "PTY helper never printed READY: " .. tostring(ready))

            local t0 = mare.task.clock()
            local res = term:send({
                terminal_id = s.terminal_id,
                text = "silent",
                submit = true,
                wait_ms = 4000,
            })
            local elapsed = mare.task.clock() - t0
            assert(res.timed_out == false, "silent OSC 133 command must wake terminal.send without timing out (elapsed=" .. tostring(elapsed) .. "ms)")
            assert(elapsed < 2000, "expected prompt wakeup well before 4000ms timeout, took " .. tostring(elapsed) .. "ms")
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await;
}

/// Statusline quota segments scope `llm/rate-limits` by provider, ignore windows past `reset_at_ms`,
/// and `segments.context` honors `cfg.gauge_width` instead of hardcoding 6 cells.
#[tokio::test]
async fn statusline_quota_scopes_by_provider_expires_past_reset_and_honors_gauge_width() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["statusline"])
            .faux(json!([]))
            .config(json!({ "logo": false }))
            .build()
            .await;
        expose(&h);

        let ok: bool = eval(
            &h,
            r#"
            local bars = require("statusline.bars")
            local now_ms = mare.task.now()
            local cur_prov = C.cordis:get("llm"):current().provider

            -- 1. Expired reset_at_ms must not display stale quota text.
            local S_exp = {
                limits_by_provider = {
                    [cur_prov] = {
                        primary = { used_percent = 95, window_minutes = 300, reset_at_ms = now_ms - 60000 },
                    },
                },
                jobs_running = {},
            }
            local H_exp = bars.helpers(C.cordis, S_exp)
            assert(H_exp.quota_text() == nil, "expired rate-limit window must not render; got " .. tostring(H_exp.quota_text()))

            -- 2. Provider-scoped limits_by_provider must not bleed across providers, and there is no
            -- provider-less S.limits fallback.
            local S_prov = {
                limits_by_provider = {
                    ["other-provider"] = {
                        primary = { used_percent = 96, window_minutes = 300, reset_at_ms = now_ms + 300000 },
                    },
                },
                jobs_running = {},
            }
            local H_prov = bars.helpers(C.cordis, S_prov)
            assert(H_prov.quota_text() == nil, "limits from other provider must not show on " .. tostring(cur_prov))
            S_prov.limits_by_provider[cur_prov] = {
                primary = { used_percent = 80, window_minutes = 300, reset_at_ms = now_ms + 300000 },
            }
            local qtext = H_prov.quota_text()
            assert(qtext and qtext:find("20%", 1, true), "expected current provider quota, got " .. tostring(qtext))

            -- 3. Context gauge honors gauge_width instead of hardcoding 6.
            local fake_ctx = {
                llm = { current = function() return { context_window = 100000 } end },
                on = function() end,
                get = function(_, name)
                    if name == "usage" then
                        return { context_tokens = function() return 50000 end }
                    end
                    return nil
                end,
            }
            local segs = require("statusline.segments")(
                fake_ctx,
                {
                    shown = function() return true end,
                    fmt_k = H_prov.fmt_k,
                    gauge_width = 10,
                },
                { context = "gauge", context_color = false }
            )
            local rendered = segs.context()
            local gauge_str = rendered and rendered.short and rendered.short[1] and rendered.short[1][1] or ""
            local expected_bar = ui.Gauge(0.5, 10)
            assert(gauge_str:find(expected_bar, 1, true), "expected 10-cell gauge (" .. expected_bar .. "), got " .. tostring(gauge_str))
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await;
}
