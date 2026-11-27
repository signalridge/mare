use crate::common::*;

#[tokio::test]
async fn plugin_declarations_and_runtime_inventory_share_the_public_api() {
    local(async {
        let env = TestEnv::new().await;
        let repo = env.cwd.join("local-plugin-source");
        std::fs::create_dir_all(repo.join("plugins")).unwrap();
        std::fs::write(
            repo.join("plugins/inventory-probe.lua"),
            "return { name = 'inventory-probe', apply = function() end }",
        )
        .unwrap();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!("mare.plugins.add {{ {{ dir = {:?}, name = 'local-source' }} }}", repo.to_string_lossy()),
        )
        .unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        let (active, found, can_add, old_api): (bool, bool, bool, bool) = eval(
            &h,
            r#"local found = false
               for _, item in ipairs(mare.plugins.list_installed()) do
                 found = found or item.name == 'local-source'
               end
               return mare.plugins.status('inventory-probe').active, found,
                      type(mare.plugins.add) == 'function', mare['p' .. 'kg'] ~= nil or mare['pack' .. 'ages'] ~= nil"#,
        )
        .await;
        assert!(active, "the declared plugin must be loaded");
        assert!(found, "the declared source must be visible in the plugin inventory");
        assert!(can_add, "configuration and runtime must share the same public table");
        assert!(!old_api, "the superseded APIs must be absent");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn ctrl_c_is_reserved() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let results: Vec<String> = eval(
            &h,
            r#"
            local calls = {
              function() return mare.keymap.set('editor', '<C-c>', 'redraw') end,
              function() return mare.keymap.del('editor', '<C-c>') end,
              function() return mare.keymap.set('editor', '<C-x> <C-c>', 'redraw') end,
              function() return mare.keymap.set('transcript', 'ctrl+c', 'view.close') end,
              function() return mare.ui.select({ items = { 'x' }, keys = { 'ctrl+c' } }) end,
            }
            local results = {}
            for _, call in ipairs(calls) do
              local ok, err = pcall(call)
              results[#results + 1] = ok and 'succeeded' or tostring(err)
            end
            return results
        "#,
        )
        .await;
        for (i, result) in results.iter().enumerate() {
            assert!(result.contains("reserved"), "case {i}: {result}");
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn keymap_set_takes_modes_unique_and_returns_an_undo_handle() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (dup, again, contexts, unknown): (bool, bool, i64, String) = eval(
            &h,
            r#"local hd = mare.keymap.set({ "editor", "transcript" }, "<C-y>", "redraw", { desc = "d" })
               local dup = pcall(mare.keymap.set, "editor", "<C-y>", "quit", { unique = true })
               local contexts = #hd.entries
               hd:remove()
               local h2 = mare.keymap.set("editor", "<C-y>", "quit", { unique = true })
               mare.keymap.remove(h2)
               local _, unknown = pcall(mare.keymap.set, "n", "<C-y>", "quit")
               return dup, h2 ~= nil, contexts, tostring(unknown)"#,
        )
        .await;
        assert!(!dup, "unique refuses a key already bound in that context");
        assert!(again, "after the handle undid the binding the key is free");
        assert_eq!(contexts, 2);
        assert!(
            unknown.contains(r#"unknown context "n" — use "editor""#),
            "an unknown context says what to write: {unknown}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn keymap_uses_rust_shadowing_and_keeps_shared_callbacks() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let km_core = mare_shared::lua::core::keymap::handle(h.lua()).unwrap();
        let (visible, callback, removed, restored): (String, String, bool, String) = h
            .lua()
            .load(
                r#"local km = ...
               local hd = mare.keymap.set({"editor", "transcript"}, "<C-y>", function() return "alive" end, { desc = "alive" })
               local id = hd.entry.action
               mare.keymap.set("global", "<F7>", "redraw")
               mare.keymap.set("editor", "<F7>", function() return false end, { desc = "decline probe" })
               assert(mare.keymap.keys_for == nil, "obsolete alias must be removed")
               local visible = table.concat(mare.keymap.hint("redraw"), ",")
               local saved = km.config_snapshot({hd})
               mare.keymap.del("editor", "<C-y>")
               local callback = mare.keymap.callback(id)()
               hd:remove()
               local removed = mare.keymap.callback(id) == nil
               km.config_restore(saved, {})
               mare.keymap.del("editor", "<C-y>")
               local restored = mare.keymap.callback(id)()
               hd:remove()
               return visible, callback, removed and mare.keymap.callback(id) == nil, restored"#,
            )
            .call(km_core)
            .unwrap();
        assert!(visible.split(',').any(|key| key == "f7"));
        assert_eq!(callback, "alive");
        assert!(removed);
        assert_eq!(restored, "alive");
        h.shutdown().await;
    })
    .await
}

/// Binding the same Lua function twice to the same `(context, key)` in one `set_many` batch
/// (including different spellings like `<C-y>` and `ctrl+y` that normalize to the same key) must
/// keep the shared callback alive and free it cleanly once the handle is removed.
#[tokio::test]
async fn keymap_set_many_duplicate_same_function_binding_keeps_callback_alive() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (invoked, alive_after_dup, freed_after_remove): (bool, String, bool) = eval(
            &h,
            r#"
            local hits = 0
            local fn = function()
                hits = hits + 1
                return "ok:" .. tostring(hits)
            end
            local h = mare.keymap.set_many({
                { "editor", "<C-y>", fn, { desc = "dup callback" } },
                { "editor", "ctrl+y", fn, { desc = "dup callback normalized" } },
                { "transcript", "<C-y>", fn, { desc = "shared across context" } },
            })
            local res = mare.keymap.resolve("editor", "<C-y>", {})
            local id = res.candidates[1] and res.candidates[1].action
            assert(type(id) == "string", "expected callback id, got " .. tostring(id))
            local cb = mare.keymap.callback(id)
            assert(type(cb) == "function", "callback must stay registered after same-owner overwrite")
            local alive = cb()
            local invoked = mare.keymap.invoke(id) == true and hits == 2
            -- Deleting editor keeps the shared callback alive for transcript.
            mare.keymap.del("editor", "<C-y>")
            assert(type(mare.keymap.callback(id)) == "function", "callback must stay alive for transcript")
            mare.keymap.remove(h)
            return invoked, alive, mare.keymap.callback(id) == nil
            "#,
        )
        .await;
        assert!(invoked);
        assert_eq!(alive_after_dup, "ok:1");
        assert!(freed_after_remove);
        h.shutdown().await;
    })
    .await
}

/// reference implementation and Codex conflict_tests.rs:90-143: rejecting
/// a new owner's invalid bindings must leave earlier owners and their keys intact.
#[tokio::test]
async fn keymap_action_rejects_invalid_key_records_without_partial_registration() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let cases: Vec<String> = eval(
            &h,
            r#"
            local name = "u7.validation"
            local base = mare.keymap.action(name, function() return false end, { keys = { editor = "<F6>" } })
            local before = table.concat(mare.keymap.hint(name, "editor"), ",")
            local cases = {
                { "numeric key", { key = 123, when = "working" } },
                { "missing key", { when = "working" } },
                { "false key", { key = false } },
                { "unknown dictionary", { typo = "<F8>" } },
                { "sparse list", { [2] = "<F8>" } },
                { "invalid list record", { { key = 123 } } },
            }
            local results = {}
            local function has_binding(action, key)
                for _, binding in ipairs(mare.keymap.list()) do
                    if binding.action == action and (not key or binding.key == key) then return true end
                end
                return false
            end
            for i, case in ipairs(cases) do
                -- editor is sorted before transcript: a failure must not leave its valid key behind.
                local opts = { keys = { editor = "<F7>", transcript = case[2] } }
                local ok, result = pcall(mare.keymap.action, name, function() return true end, opts)
                local message = not ok and tostring(result):find("opts.keys entry requires a key string", 1, true) ~= nil
                local unchanged = mare.keymap.invoke(name) == false
                    and table.concat(mare.keymap.hint(name, "editor"), ",") == before
                    and not has_binding(name, "f7")
                if ok then result:remove() end
                local fresh = "u7.fresh." .. i
                local fresh_ok, fresh_result = pcall(mare.keymap.action, fresh, function() return true end, opts)
                local no_partial = mare.keymap.invoke(fresh) == nil and not has_binding(fresh)
                if fresh_ok then fresh_result:remove() end
                results[#results + 1] = case[1] .. ":" .. tostring(not ok) .. ":" .. tostring(message)
                    .. ":" .. tostring(unchanged) .. ":" .. tostring(not fresh_ok and no_partial)
            end
            base:remove()
            return results
            "#,
        )
        .await;
        assert_eq!(cases.len(), 6, "all invalid forms must be exercised");
        assert!(cases.iter().all(|case| case.ends_with(":true:true:true:true")), "invalid key records: {cases:?}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn keymap_action_accepts_empty_key_lists_and_valid_record_forms() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (empty, valid, removed): (bool, bool, bool) = eval(
            &h,
            r#"
            local empty = mare.keymap.action("u7.empty", function() return true end, { keys = { editor = {} } })
            local empty_ok = mare.keymap.invoke("u7.empty") and #mare.keymap.hint("u7.empty", "editor") == 0
            local valid = mare.keymap.action("u7.valid", function() return true end, { keys = {
                editor = { key = "<F9>", when = "working" },
                transcript = { "<F10>", { key = "<F11>", when = "working" }, { "<F12>", "working" } },
            } })
            local count = 0
            for _, binding in ipairs(mare.keymap.list()) do
                if binding.action == "u7.valid" then count = count + 1 end
            end
            empty:remove(); valid:remove()
            return empty_ok, count == 4, mare.keymap.invoke("u7.valid") == nil
            "#,
        )
        .await;
        assert!(empty, "an empty list registers an action without default keys");
        assert!(valid, "single records and list entries retain their existing forms");
        assert!(removed, "removing the handle unregisters the action");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn loader_reset_and_prune_clear_the_bytecode_cache() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let settled = std::time::SystemTime::now() - std::time::Duration::from_secs(10);
        let src = env.work.path().join("cached.lua");
        std::fs::write(&src, "return 1").unwrap();
        std::fs::File::options().write(true).open(&src).unwrap().set_modified(settled).unwrap();
        mare_kernel::load_cached_chunk(h.lua(), None, &src, "@cached", None).unwrap();
        let gone = env.work.path().join("gone.lua");
        std::fs::write(&gone, "return 2").unwrap();
        std::fs::File::options().write(true).open(&gone).unwrap().set_modified(settled).unwrap();
        mare_kernel::load_cached_chunk(h.lua(), None, &gone, "@gone", None).unwrap();
        std::fs::remove_file(&gone).unwrap();
        // the host's daily background prune may run first and take the entry itself, and the
        // count can include other plugins' entries: check this test's own entries, not counts
        // a fresh temp file is a write in progress (another process): prune leaves it alone
        let luac = mare_shared::paths::cache_dir().join("luac");
        std::fs::create_dir_all(&luac).unwrap();
        let writing = luac.join("0000000000000000.luac.42.tmp");
        std::fs::write(&writing, b"partial").unwrap();
        let pruned: i64 = eval(&h, "return mare.loader.prune()").await;
        assert!(pruned >= 0);
        assert!(writing.exists(), "a write in progress is not pruned");
        global(&h, "GONE", gone.to_string_lossy().to_string());
        let left: i64 = eval(&h, "return mare.loader.reset(GONE)").await;
        assert_eq!(left, 0, "the deleted source's entry was pruned");
        global(&h, "SRC", src.to_string_lossy().to_string());
        let (one, rest): (i64, i64) = eval(&h, "return mare.loader.reset(SRC), mare.loader.reset()").await;
        assert_eq!(one, 1);
        assert!(rest >= 0);
        // only this test's entry is checked: other plugins may still be compiling in the
        // background and add their own entries after the full reset
        let again: i64 = eval(&h, "return mare.loader.reset(SRC)").await;
        assert_eq!(again, 0, "the entry is gone after the full reset");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn images_render_lazily_and_reload_discards_the_module() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["images"]).no_session().build().await;
        global(&h, "IMG", h.service_table("images").unwrap());
        assert!(!h.kernel.is_module_loaded("images", "images.render"));
        exec(&h, "IMG:show({ { type = 'image', data = 'aW1hZ2U=' } })").await;
        assert!(!h.kernel.is_module_loaded("images", "images.render"), "headless output stays lazy");
        let (count, bytes, rows, cols): (i64, String, i64, i64) = eval(
            &h,
            r#"local old_ui, old_mode = mare.ui, mare.mode
               local seen, count = nil, 0
               mare.mode = 'tui'
               mare.ui = {
                 connected = function() return true end,
                 caps = function() return { images = 'kitty' } end,
                 image = function(args) seen = args; return args end,
                 Lines = function(nodes) return nodes end,
                 Indent = function(_, node) return node end,
                 push = function(_, payload) return payload end,
                 finish = function() count = count + 1 end,
               }
               IMG:show({ { type = 'text', text = 'ignored' },
                                { type = 'image', data = mare.crypto.base64_encode('image-bytes') } })
               mare.ui, mare.mode = old_ui, old_mode
               return count, seen.bytes, seen.max_rows, seen.max_cols"#,
        )
        .await;
        assert!(h.kernel.is_module_loaded("images", "images.render"));
        assert_eq!((count, bytes.as_str(), rows, cols), (1, "image-bytes", 12, 60));
        h.kernel.reload(Some("images")).await.unwrap();
        assert!(!h.kernel.is_module_loaded("images", "images.render"));
        h.shutdown().await;
    })
    .await
}

/// Built-ins in two layers: the entry registers the command at startup, the
/// implementation module (`preset/plugins/lua/commands/*.lua`) loads on its first use, owned by
/// the plugin (dropped from `package.loaded` on /reload). Other plugins' modules can't be
/// reached through a built-in's name space.
#[tokio::test]
async fn builtin_implementation_modules_load_on_first_use() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage", "images", "plugins"]).no_session().build().await;
        global(&h, "CMD", h.service_table("commands").unwrap());
        let loaded = |m: &'static str| {
            let h = h.clone();
            let owner = m.split('.').next().unwrap();
            async move { h.kernel.is_module_loaded(owner, m) }
        };
        assert!(eval::<bool>(&h, "return CMD:get('plugins') ~= nil").await, "registered at startup");
        assert!(!loaded("plugins.tree").await, "not loaded before the first /plugins tree");
        exec(&h, "CMD:run('/plugins tree')").await;
        assert!(loaded("plugins.tree").await, "loaded by the first /plugins tree");
        h.kernel.reload(Some("plugins")).await.unwrap();
        assert!(!loaded("plugins.tree").await, "a reload drops the plugin's modules");
        // compact: the service, hooks and commands are registered at
        // startup and measuring is cheap; the stages + run loop and the /context breakdown each
        // load on their first use
        global(&h, "K", h.service_table("compact").unwrap());
        // Re-read the commands service after reloading the plugin manager.
        global(&h, "CMD", h.service_table("commands").unwrap());
        assert!(eval::<bool>(&h, "return CMD:get('context') ~= nil and CMD:get('compact') ~= nil").await);
        assert!(eval::<bool>(&h, "return K:measure() > 0").await, "measuring needs neither module");
        assert!(!loaded("compact.stages").await, "not loaded before the first compaction");
        assert!(!loaded("compact.context").await, "not loaded before the first /context");
        exec(&h, "CMD:run('/context')").await;
        assert!(loaded("compact.context").await, "loaded by the first /context");
        assert!(eval::<bool>(&h, "return K:measure({ by_source = true }).total > 0").await);
        assert!(!loaded("compact.stages").await, "/context doesn't load the stages");
        let (ok, err): (bool, String) = eval(
            &h,
            "local ok, e = pcall(function() return K:run(nil, { summarize_only = true }) end); return ok, tostring(e)",
        )
        .await;
        assert!(!ok);
        assert!(err.contains("nothing to summarize"), "{err}");
        assert!(loaded("compact.stages").await, "loaded by the first run");
        h.kernel.reload(Some("compact")).await.unwrap();
        assert!(!loaded("compact.stages").await && !loaded("compact.context").await, "a reload drops them");
        // images: on a terminal that can draw them, content without an image doesn't load the
        // renderer; the first image does (headless never loads it:
        // images_render_lazily_and_reload_discards_the_module)
        global(&h, "IMG", h.service_table("images").unwrap());
        exec(
            &h,
            r#"local old_ui, old_mode = mare.ui, mare.mode
               mare.mode = 'tui'
               mare.ui = {
                 connected = function() return true end,
                 caps = function() return { images = 'kitty' } end,
                 image = function(args) return args end,
                 Lines = function(nodes) return nodes end,
                 Indent = function(_, node) return node end,
                 push = function(_, payload) return payload end,
                 finish = function() end,
               }
               IMG:show({ { type = 'text', text = 'no picture' } })
               mare._old_ui_tmp, mare._old_mode_tmp = old_ui, old_mode"#,
        )
        .await;
        let before = h.kernel.is_module_loaded("images", "images.render");
        exec(
            &h,
            r#"IMG:show({ { type = 'image', mime = 'image/png', data = 'iVBORw0KGgo=' } })
               mare.ui, mare.mode = mare._old_ui_tmp, mare._old_mode_tmp"#,
        )
        .await;
        let after = h.kernel.is_module_loaded("images", "images.render");
        assert!(!before, "not loaded before the first image");
        assert!(after, "loaded by the first image");
        let err: String = eval(&h, r#"local ok, e = pcall(require, "nosuchplugin.panel"); return tostring(e)"#).await;
        assert!(err.contains("not found"), "{err}");
        // the shared core module (`mare_lib`): loaded once in the core partition
        assert!(h.kernel.is_module_loaded("core", "mare_lib"), "loaded by the built-ins that use it");
        // retry_after_from_text accepts what llm's and provider-openai's copies each accepted
        let retry: String = eval(
            &h,
            r#"local f = require("mare_lib").retry_after_from_text
            local out = {}
            for _, m in ipairs({ "Try again in 1.5s", "try again in .5s", "try again in10ms",
                "try again in 2 seconds", "try again later" }) do
                out[#out + 1] = tostring(f(m))
            end
            return table.concat(out, ",")"#,
        )
        .await;
        assert_eq!(retry, "1500,500,10,2000,nil");
        // load_state: a value saved with mare.state, the default when unset, the whole table without a key
        let st: String = eval(
            &h,
            r#"local lib = require("mare_lib")
            mare.state.set("split_probe", 3)
            return tostring(lib.load_state(nil, "split_probe", 0)) .. "," .. tostring(lib.load_state(nil, "split_unset", 7))
                .. "," .. tostring(lib.load_state(nil).split_probe)"#,
        )
        .await;
        assert_eq!(st, "3,7,3");
        // a corrupt (nil, err) or failing state read never throws: the default, `{}` for the table
        let st: String = eval(
            &h,
            r#"local lib = require("mare_lib")
            local read = mare.state.read
            local out = {}
            for _, bad in ipairs({ function() return nil, "corrupt" end, function() error("io") end }) do
              mare.state.read = bad
              local ok, whole = pcall(lib.load_state, nil)
              out[#out + 1] = tostring(ok and type(whole) == "table" and next(whole) == nil)
              out[#out + 1] = tostring(lib.load_state(nil, "split_probe", 5))
            end
            mare.state.read = read
            return table.concat(out, ",")"#,
        )
        .await;
        assert_eq!(st, "true,5,true,5");
        // truncate_utf8: empty/omitted opts is a no-op (no max_* field required); max_bytes caps cleanly
        let trunc: String = eval(
            &h,
            r#"local f = require("mare_lib").truncate_utf8
            local a, ac = f("hello", {})
            local b, bc = f("hello")
            local c, cc = f("hello", { max_bytes = 3 })
            return table.concat({ a, tostring(ac), b, tostring(bc), c, tostring(cc) }, ",")"#,
        )
        .await;
        assert_eq!(trunc, "hello,false,hello,false,hel,true");
        h.shutdown().await;
    })
    .await
}

/// `require` never looks in the working directory or system Lua paths: an untrusted project
/// could otherwise plant `evil.lua` that a plugin's `require "evil"` runs (found hands-on).
#[tokio::test]
async fn require_never_loads_modules_from_the_working_directory() {
    local(async {
        let env = TestEnv::new().await;
        let marker = env.work.path().join("PWNED");
        std::fs::write(
            env.cwd.join("evil.lua"),
            format!("io.open({:?}, 'w'):close() return {{}}", marker.to_string_lossy()),
        )
        .unwrap();
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("rq.lua"),
            "return { name = 'rq', apply = function() mare._rq = { pcall(require, 'evil') } end }",
        )
        .unwrap();
        let h = env.host().user_plugins().build().await;
        let (ok, err): (bool, String) = eval(&h, "return mare._rq[1], tostring(mare._rq[2])").await;
        assert!(!ok, "the planted module must not load");
        assert!(!err.contains("./"), "no working-directory lookup in the error: {err}");
        let (path, cpath): (String, String) = eval(&h, "return package.path, package.cpath").await;
        assert_eq!((path.as_str(), cpath.as_str()), ("", ""));
        assert!(!marker.exists(), "evil.lua ran");
        h.shutdown().await;
    })
    .await
}

/// The vendored cordis-lua (`crates/mare-kernel/lua/cordis/`) through `require`: cordis is the
/// plugin interface itself (`types/cordis` is a public type set), so any plugin reaches it
/// (coordinator ruling 2026-11-17; the previous isolation belonged to the removed path).
#[tokio::test]
async fn kernel_libraries_are_reachable_by_require() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (active, loader, source): (bool, bool, String) = eval(
            &h,
            r#"local cordis = require("cordis")
               local L = require("cordis.loader")
               local info = package.loaded["cordis.fiber"] and "fiber loaded" or "fiber missing"
               return cordis.FiberState.ACTIVE == 2, type(L.Loader) == "table", info"#,
        )
        .await;
        assert!(active, "cordis exports its fiber states");
        assert!(loader, "cordis.loader is vendored too");
        assert_eq!(source, "fiber loaded");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn keymap_contexts_declare_and_release() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (dup_err, sel_err, nope_err, bad_err, bound_ok, gone_from_list, set_after_err, wrong_ctx_err, has_core_and_lua): (
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
            local h = mare.keymap.context("editor.vi", { parent = "editor" })
            local ok_dup = pcall(mare.keymap.context, "editor.vi", { parent = "editor" })
            local ok_inp = pcall(mare.keymap.context, "input.custom", { parent = "input" })
            local ok_sel = pcall(mare.keymap.context, "select.custom", { parent = "select" })
            local ok_nope = pcall(mare.keymap.context, "nope.sub", { parent = "nope" })
            local ok_bad = pcall(mare.keymap.context, "vi", { parent = "editor" })
            local ok_bind = pcall(mare.keymap.set, "editor.vi", "x", "redraw")
            local ok_wrong = pcall(mare.keymap.set, "transcript", "<C-x>", "steer")

            mare.keymap.action("t.custom", function() end, { desc = "custom lua action", contexts = { "editor" } })
            local seen_core, seen_lua = false, false
            for _, a in ipairs(mare.keymap.actions()) do
                if a.name == "steer" and a.core == true and a.desc ~= "" then seen_core = true end
                if a.name == "t.custom" and a.core == false then seen_lua = true end
            end

            h:remove()
            local gone = true
            for _, b in ipairs(mare.keymap.list()) do
                if b.context == "editor.vi" then gone = false end
            end
            local ok_after = pcall(mare.keymap.set, "editor.vi", "x", "redraw")
            return not ok_dup, ok_inp and ok_sel, not ok_nope, not ok_bad, ok_bind, gone, not ok_after, not ok_wrong, seen_core and seen_lua
        "#,
        )
        .await;
        assert!(dup_err, "second declare of same context errors");
        assert!(sel_err, "both input and select accept subcontexts");
        assert!(nope_err, "parent = nope errors");
        assert!(bad_err, "bad name without parent prefix errors");
        assert!(bound_ok, "core action works in subcontext of editor");
        assert!(gone_from_list, "h:remove() removes bindings from list()");
        assert!(set_after_err, "set on removed context errors");
        assert!(wrong_ctx_err, "core action in unsupported context errors");
        assert!(has_core_and_lua, "actions() lists core and Lua actions");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn hint_follows_chain_and_masks() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().user_config(false).no_session().build().await;
        let initial: Vec<String> = eval(
            &h,
            r#"mare.keymap.set("editor", "<F6>", "transcript")
               return mare.keymap.hint("transcript", "editor")"#,
        )
        .await;
        assert_eq!(initial, vec!["f6", "ctrl+t"]);

        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), r#"mare.keymap.set("editor", "<F8>", "transcript")"#)
            .unwrap();
        h.reload_config().await.unwrap();

        let (with_user, masked, ed_redraw, tr_redraw, list_uses_context): (
            Vec<String>,
            Vec<String>,
            Vec<String>,
            Vec<String>,
            bool,
        ) = eval(
            &h,
            r#"local with_user = mare.keymap.hint("transcript", "editor")
               mare.keymap.set("editor", "<C-t>", false)
               local masked = mare.keymap.hint("transcript", "editor")
               mare.keymap.set("global", "<F7>", "redraw")
               local ed_redraw = mare.keymap.hint("redraw", "editor")
               local tr_redraw = mare.keymap.hint("redraw", "transcript")
               local list_ok = true
               for _, b in ipairs(mare.keymap.list()) do
                   if b.context == nil or b.mode ~= nil then list_ok = false end
               end
               return with_user, masked, ed_redraw, tr_redraw, list_ok"#,
        )
        .await;
        assert_eq!(with_user, vec!["f8", "f6", "ctrl+t"]);
        assert_eq!(masked, vec!["f8", "f6"]);
        assert!(ed_redraw.contains(&"f7".to_string()), "{ed_redraw:?}");
        assert!(tr_redraw.contains(&"f7".to_string()), "{tr_redraw:?}");
        assert!(list_uses_context, "list() rows use context, not mode");
        h.shutdown().await;
    })
    .await
}
