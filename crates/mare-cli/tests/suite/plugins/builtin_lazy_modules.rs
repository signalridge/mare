//! Built-in plugins load their cold submodules on first use: startup registers only the entry
//! points, and the first call that needs a submodule loads it (the `mare_lib.lazy` /
//! `mare_lib.lazy_group` pattern third-party plugins use too).

use mare_host::Host;
use serde_json::json;

use crate::common::*;

fn assert_cold(h: &Host, owner: &str, modules: &[&str], when: &str) {
    for module in modules {
        assert!(!h.kernel.is_module_loaded(owner, module), "{module} must not be loaded {when}");
    }
}

fn assert_warm(h: &Host, owner: &str, modules: &[&str], when: &str) {
    for module in modules {
        assert!(h.kernel.is_module_loaded(owner, module), "{module} must be loaded {when}");
    }
}

#[tokio::test]
async fn agent_loads_its_run_loop_modules_on_the_first_prompt() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "hello back" }])).build().await;
        expose(&h);
        let cold = ["agent.persist", "agent.tool_exec", "agent.inputs", "agent.step", "agent.loop"];
        assert_cold(&h, "agent", &cold, "after startup");
        // the transcript baseline a frontend reads on attach does not need them either
        exec(&h, "A:baseline_cut()").await;
        assert_cold(&h, "agent", &cold, "after reading the baseline");
        let text: String = eval(&h, r#"return A:wait(A:prompt("hi").turn_id).text"#).await;
        assert_eq!(text, "hello back");
        assert_warm(&h, "agent", &cold, "after the first prompt");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn permissions_loads_its_vote_path_on_the_first_tool_call() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "x.txt", "content": "hi" } }] },
            { "text": "done" }
        ]);
        let h = env
            .host()
            .faux(script)
            .config(json!({ "plugins": { "permissions": { "enabled": true } } }))
            .args(json!({ "no_session": true }))
            .build()
            .await;
        expose(&h);
        let cold = ["permissions.argv", "permissions.rules", "permissions.sensitive", "permissions.vote"];
        assert_cold(&h, "permissions", &cold, "after startup");
        // untrusted -> read-only: the write is voted on and asks; nobody can answer, so it is denied
        h.prompt("write it", json!({})).await.unwrap();
        assert!(tool_results(&h).await.contains("Tool call denied"));
        assert!(!env.cwd.join("x.txt").exists());
        assert_warm(&h, "permissions", &["permissions.vote", "permissions.rules"], "after the first tool call");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn permissions_validates_configured_rules_and_patterns_during_startup() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([]))
            .config(json!({ "plugins": { "permissions": {
                "enabled": true,
                "rules": [{ "argv": ["git", "status"], "decision": "allow" }],
                "sensitive": { "patterns": ["~/.secrets/**"] }
            } } }))
            .args(json!({ "no_session": true }))
            .build()
            .await;
        // configured rules and credential patterns are checked in apply, so a bad one still fails
        // startup rather than the first tool call
        assert_warm(&h, "permissions", &["permissions.rules", "permissions.sensitive"], "when rules are configured");
        assert_cold(&h, "permissions", &["permissions.vote"], "before any tool call");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn commands_loads_its_availability_checks_on_the_first_run() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).no_session().build().await;
        expose(&h);
        assert_cold(&h, "commands", &["commands.availability"], "after startup");
        exec(
            &h,
            r#"
            local commands = C.cordis:get("commands")
            -- registration still validates the availability condition names
            assert(not pcall(commands.register, commands, "probe-bad", {
                description = "x", availability = "sometimes", run = function() end,
            }))
            "#,
        )
        .await;
        assert_cold(&h, "commands", &["commands.availability"], "after registering a command");
        exec(&h, r#"C.cordis:get("commands"):run("/status")"#).await;
        assert_warm(&h, "commands", &["commands.availability"], "after the first command run");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn context_loads_instruction_files_and_the_report_on_first_use() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("AGENTS.md"), "Always answer in haiku.\n").unwrap();
        let h = env.host().faux(json!([])).no_session().build().await;
        expose(&h);
        let cold = ["context.instructions", "context.report"];
        assert_cold(&h, "context", &cold, "after startup");
        let has_agents: bool = eval(
            &h,
            r#"
            local text = C.cordis:get("context"):system_prompt()
            return text:find("Always answer in haiku.", 1, true) ~= nil
            "#,
        )
        .await;
        assert!(has_agents, "AGENTS.md must still reach the system prompt");
        assert_warm(&h, "context", &["context.instructions"], "after the first prompt build");
        assert_cold(&h, "context", &["context.report"], "before the first report");
        exec(&h, r#"assert(C.cordis:get("context"):report().total > 0)"#).await;
        assert_warm(&h, "context", &["context.report"], "after the first report");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mcp_loads_its_actions_catalog_and_result_shaping_on_first_use() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([]))
            .config(json!({ "plugins": { "mcp": { "enabled": true } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        let cold = ["mcp.results", "mcp.elicit", "mcp.prompts", "mcp.actions", "mcp.catalog"];
        assert_cold(&h, "mcp", &cold, "after startup with no servers");
        exec(
            &h,
            r#"
            local mcp = C.cordis:get("mcp")
            local listed = mcp:list_servers()
            assert(type(listed.data) == "table" and #listed.data == 0)
            assert(type(listed.errors) == "table" and #listed.errors == 0)
            assert(type(mcp:catalog()) == "table")
            "#,
        )
        .await;
        // with no server configured, listing servers (the status bar asks on every draw) does not
        // need the actions module
        assert_cold(&h, "mcp", &["mcp.actions"], "after listing no servers");
        assert_warm(&h, "mcp", &["mcp.catalog"], "after reading the catalog");
        exec(&h, r#"assert(not pcall(C.cordis:get("mcp").connect, C.cordis:get("mcp"), "missing"))"#).await;
        assert_warm(&h, "mcp", &["mcp.actions"], "after the first server action");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn context_scans_skills_on_first_use_and_for_skill_commands() {
    local(async {
        let env = TestEnv::new().await;
        // without the commands service nothing needs the skill list before the first prompt
        let h = env
            .host()
            .faux(json!([]))
            .no_session()
            .config(json!({ "plugins": { "commands": { "enabled": false } } }))
            .build()
            .await;
        expose(&h);
        assert_cold(&h, "context", &["context.skills_scan"], "after a start without commands");
        exec(&h, r#"C.cordis:get("context"):system_prompt()"#).await;
        assert_warm(&h, "context", &["context.skills_scan"], "after the first prompt build");
        h.shutdown().await;
        // with it, the /skill:<name> commands are registered from the scan when it comes up
        let h = env.host().faux(json!([])).no_session().build().await;
        assert_warm(&h, "context", &["context.skills_scan"], "once the commands service is up");
        h.shutdown().await;
    })
    .await
}

/// Calling `llm:set_effort(..., { scope_name = ..., save = true })` and `llm:clear_scope(...)`
/// before `llm.params_api` has been loaded must transparently load `D.remember_scoped_param`
/// instead of failing with a nil call.
#[tokio::test]
async fn llm_set_effort_scoped_save_works_before_params_api_loads() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).no_session().build().await;
        expose(&h);
        assert_cold(&h, "llm", &["llm.params_api"], "after startup");

        let scoped: String = eval(
            &h,
            r#"
            local llm = C.cordis:get("llm")
            llm:set_effort("low", { scope_name = "plan", save = true })
            return llm:scoped_effort("plan") or ""
            "#,
        )
        .await;
        assert_eq!(scoped, "low");
        assert_warm(&h, "llm", &["llm.params_api"], "after scoped set_effort with save=true");

        exec(
            &h,
            r#"
            local llm = C.cordis:get("llm")
            llm:clear_scope("plan", nil, { save = true })
            assert(llm:scoped_effort("plan") == nil)
            "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

/// Triggering `tools.bash_run` (e.g. via `session/loaded` + `compact/completed`) on a fresh host
/// before `tools:execute(...)` has run must not fail when `tools.bash_persistent` or
/// `tools.bash_output` accesses `D.approval` / `D.pipeline`.
#[tokio::test]
async fn tools_bash_run_works_before_execute_loads_pipeline() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).no_session().build().await;
        expose(&h);
        assert_cold(
            &h,
            "tools",
            &["tools.approval", "tools.results", "tools.pipeline", "tools.nested", "tools.bash_run"],
            "after startup",
        );

        let other_dir = env.cwd.join("sub");
        std::fs::create_dir_all(&other_dir).unwrap();
        let other = other_dir.to_string_lossy().replace('\\', "/");
        let errs: Vec<String> = eval(
            &h,
            &format!(
                r#"
                local e1 = C.cordis.bus:emit("session/loaded", {{ shell_cwd = {other:?} }}) or {{}}
                local e2 = C.cordis.bus:emit("compact/completed", {{}}) or {{}}
                local out = {{}}
                for _, item in ipairs(e1) do out[#out + 1] = tostring(item[2]) end
                for _, item in ipairs(e2) do out[#out + 1] = tostring(item[2]) end
                return out
                "#
            ),
        )
        .await;
        assert!(errs.is_empty(), "bash_run lazy load before tools:execute must not error: {errs:?}");
        assert_warm(&h, "tools", &["tools.bash_run", "tools.approval"], "after compact/completed loads bash_run");
        h.shutdown().await;
    })
    .await
}

unsafe extern "C-unwind" fn test_lua_getupvalue(state: *mut mlua::lua_State) -> std::ffi::c_int {
    unsafe {
        if mlua::ffi::lua_type(state, 1) != mlua::ffi::LUA_TFUNCTION {
            return 0;
        }
        let idx = mlua::ffi::lua_tointeger(state, 2) as std::ffi::c_int;
        let name = mlua::ffi::lua_getupvalue(state, 1, idx);
        if name.is_null() {
            return 0;
        }
        mlua::ffi::lua_pushstring(state, name);
        mlua::ffi::lua_insert(state, -2);
        2
    }
}

/// Generic regression guard across all built-in plugins: instrument `mare_lib.lazy` and
/// `mare_lib.lazy_group` before a full plugin reload, then force-load every registered lazy
/// loader and fail if any captured shared table gains a new function (or function-bearing table)
/// key that had no stub before the lazy module loaded.
#[tokio::test]
async fn all_builtin_lazy_modules_stub_shared_table_exports() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().faux(json!([])).no_session().config(json!({
            "plugins": {
                "permissions": { "enabled": true },
                "mcp": { "enabled": true },
            }
        }));
        b.opts.mode = mare_shared::ui::Mode::Tui;
        let h = b.build().await;

        {
            let lua = h.kernel.lua();
            let mare_tbl: mlua::Table = lua.globals().get("mare").unwrap();
            let getup = unsafe { lua.create_c_function(test_lua_getupvalue) }.unwrap();
            let fn_src = lua
                .create_function(|_, f: mlua::Function| {
                    let info = f.info();
                    Ok(format!(
                        "{}:{}",
                        info.short_src.as_deref().unwrap_or("?"),
                        info.line_defined.unwrap_or(0)
                    ))
                })
                .unwrap();
            mare_tbl.set("_test_getupvalue", getup).unwrap();
            mare_tbl.set("_test_fn_src", fn_src).unwrap();
        }

        exec(
            &h,
            r#"
            -- Force connected UI path on reload so ui.lua registers its lazy transcript loader too.
            mare.ui.connected = function()
                return true
            end
            local lib = require("mare_lib")
            local entries = {}
            mare._lazy_audit_entries = entries
            local orig_lazy = lib.lazy
            local orig_lazy_group = lib.lazy_group
            local getupvalue = mare._test_getupvalue
            local fn_src = mare._test_fn_src

            local function collect_tables(fn, extra_tables)
                local out, seen = {}, {}
                local function add(name, val)
                    if type(val) ~= "table" or seen[val] then
                        return
                    end
                    if val == mare or val == _G or val == lib or val == package.loaded or val == entries then
                        return
                    end
                    seen[val] = true
                    out[#out + 1] = { name = name, tbl = val }
                end
                if extra_tables then
                    for _, item in ipairs(extra_tables) do
                        add(item.name, item.tbl)
                    end
                end
                local i = 1
                while true do
                    local uname, uval = getupvalue(fn, i)
                    if not uname then
                        break
                    end
                    if uname ~= "_ENV" then
                        add(uname, uval)
                    end
                    i = i + 1
                end
                return out
            end

            lib.lazy = function(loader)
                local ensure = orig_lazy(loader)
                entries[#entries + 1] = {
                    kind = "lazy",
                    src = fn_src(loader),
                    ensure = ensure,
                    tables = collect_tables(loader),
                }
                return ensure
            end

            lib.lazy_group = function(target, methods, loader, opts)
                local ensure, proxy = orig_lazy_group(target, methods, loader, opts)
                local extras = { { name = "<target>", tbl = target } }
                if opts and type(opts.extra_target) == "table" then
                    extras[#extras + 1] = { name = "<extra_target>", tbl = opts.extra_target }
                end
                entries[#entries + 1] = {
                    kind = "lazy_group",
                    src = fn_src(loader),
                    ensure = ensure,
                    tables = collect_tables(loader, extras),
                }
                return ensure, proxy
            end
            "#,
        )
        .await;

        let report = h.kernel.reload_report(None).await.unwrap();
        assert!(report.failures.is_empty(), "full reload under lazy check must succeed: {:?}", report.failures);

        let (checked, violations): (i64, Vec<String>) = eval(
            &h,
            r#"
            local entries = mare._lazy_audit_entries or {}
            local violations = {}

            local function has_callable_content(t)
                local mt = getmetatable(t)
                if type(mt) == "table" and type(mt.__call) == "function" then
                    return true
                end
                for _, v in pairs(t) do
                    if type(v) == "function" then
                        return true
                    end
                end
                return false
            end

            local i = 1
            while i <= #entries do
                local e = entries[i]
                local before, before_sub = {}, {}
                for ti, rec in ipairs(e.tables) do
                    local snap, subs = {}, {}
                    for k, v in pairs(rec.tbl) do
                        local tv = type(v)
                        snap[k] = tv
                        if tv == "table" and getmetatable(v) == nil then
                            local sub = {}
                            for sk, sv in pairs(v) do
                                sub[sk] = type(sv)
                            end
                            subs[k] = sub
                        end
                    end
                    before[ti] = snap
                    before_sub[ti] = subs
                end
                local ok, err = pcall(e.ensure)
                if not ok then
                    violations[#violations + 1] = e.src .. " (" .. e.kind .. "): loader failed: " .. tostring(err)
                else
                    for ti, rec in ipairs(e.tables) do
                        local snap, subs = before[ti], before_sub[ti]
                        for k, v in pairs(rec.tbl) do
                            local tv = type(v)
                            if snap[k] == nil then
                                if tv == "function" then
                                    violations[#violations + 1] =
                                        e.src .. " (" .. e.kind .. "): added unstubbed function `" .. rec.name .. "." .. tostring(k) .. "`"
                                elseif tv == "table" and has_callable_content(v) then
                                    violations[#violations + 1] =
                                        e.src .. " (" .. e.kind .. "): added unstubbed function-table `" .. rec.name .. "." .. tostring(k) .. "`"
                                end
                            elseif tv == "table" and subs[k] ~= nil then
                                local sub_before = subs[k]
                                for sk, sv in pairs(v) do
                                    if sub_before[sk] == nil and type(sv) == "function" then
                                        violations[#violations + 1] =
                                            e.src .. " (" .. e.kind .. "): added unstubbed function `" .. rec.name .. "." .. tostring(k) .. "." .. tostring(sk) .. "`"
                                    end
                                end
                            end
                        end
                    end
                end
                i = i + 1
            end
            table.sort(violations)
            return #entries, violations
            "#,
        )
        .await;
        h.shutdown().await;

        assert!(checked >= 20, "expected at least 20 built-in lazy loaders to be audited, got {checked}");
        assert!(
            violations.is_empty(),
            "built-in lazy modules added unstubbed shared-table exports ({} violations across {checked} loaders):\n{}",
            violations.len(),
            violations.join("\n")
        );
    })
    .await
}
