//! `mare.ui.override(name, fn(opts, next))` and `ctx.ui:override` for
//! `select`, `input`, `confirm`, and `notify`.

use super::*;

/// Validation + overriding all four UI surfaces (`select`, `input`, `confirm`, `notify`) via
/// `mare.ui.override(name, fn(opts, next))`.
#[tokio::test]
async fn ui_override_validates_and_wraps_all_four_surfaces() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);

        exec(
            &t.vt.host,
            r#"
            assert(not pcall(function() mare.ui.override("editor", function(_, next) return next() end) end))
            assert(not pcall(function() mare.ui.override("unknown", function(_, next) return next() end) end))
            assert(not pcall(function() mare.ui.override("select", "not-a-fn") end))

            _G.SEL_CALLS = 0
            _G.SEL_TITLES = {}
            local rel_sel = mare.ui.override("select", function(opts, next)
                _G.SEL_CALLS = _G.SEL_CALLS + 1
                _G.SEL_TITLES[_G.SEL_CALLS] = tostring(opts.title)
                opts.title = "[O] " .. tostring(opts.title)
                return next(opts)
            end)
            local rel_inp = mare.ui.override("input", function(opts, next)
                opts.title = "[I] " .. tostring(opts.title)
                local v, err = next(opts)
                if v ~= nil then
                    return v .. "!", err
                end
                return v, err
            end)
            local rel_conf = mare.ui.override("confirm", function(opts, next)
                assert(opts.prompt == "Delete item?" and opts.default == false)
                opts.prompt = "Wrapped: " .. opts.prompt
                return next(opts)
            end)
            local rel_not = mare.ui.override("notify", function(opts, next)
                assert(opts.level == "warning" or opts.level == "info")
                opts.text = "NOTICE-PREFIX: " .. tostring(opts.text)
                return next(opts)
            end)
            _G.RELS = { rel_sel, rel_inp, rel_conf, rel_not }

            mare.ui.notify("disk warm", "warning")
            "#,
        )
        .await;

        t.until("overridden notify reaches screen before opening modals", |s| s.contains("NOTICE-PREFIX: disk warm"))
            .await;

        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 R_SEL = { mare.ui.select({ title = "PickOne", items = { "one", "two" } }) }
               end)"#,
        )
        .await;

        t.until("select shows overridden title", |s| s.contains("PickOne")).await;
        let sel_calls: i64 = eval(&t.vt.host, "return _G.SEL_CALLS").await;
        let sel_titles: String = eval(&t.vt.host, "return table.concat(_G.SEL_TITLES, '|')").await;
        assert_eq!(sel_calls, 1, "expected 1 call, got {sel_calls}: [{sel_titles}]\nscreen:\n{}", t.screen());
        assert!(t.screen().contains("[O] PickOne"), "screen:\n{}", t.screen());
        assert!(!t.screen().contains("[O] [O] PickOne"), "unexpected double prefix:\n{}", t.screen());
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("select closed", |s| !s.contains("[O] PickOne")).await;
        assert_eq!(eval::<Option<i64>>(&t.vt.host, "return R_SEL and R_SEL[1]").await, Some(2));

        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 R_INP = { mare.ui.input({ title = "AskName", default = "ada" }) }
               end)"#,
        )
        .await;
        t.until("input shows overridden title", |s| s.contains("[I] AskName")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("input closed", |s| !s.contains("[I] AskName")).await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return R_INP and R_INP[1]").await.as_deref(), Some("ada!"));

        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 R_CONF = { mare.ui.confirm("Delete item?", false) }
               end)"#,
        )
        .await;
        t.until("confirm shows overridden prompt", |s| s.contains("Wrapped: Delete item?")).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.until("confirm closed", |s| !s.contains("Wrapped: Delete item?")).await;
        assert_eq!(eval::<Option<bool>>(&t.vt.host, "return R_CONF and R_CONF[1]").await, Some(true));

        exec(
            &t.vt.host,
            r#"for _, r in ipairs(_G.RELS) do r() end
               mare.ui.notify("after release raw", "info")"#,
        )
        .await;
        t.until("unwrapped notification appears after release", |s| {
            s.contains("after release raw") && !s.contains("NOTICE-PREFIX: after release raw")
        })
        .await;
    })
    .await
}

/// Stacking multiple overrides, releasing a middle layer, self-release inside `fn`, and
/// idempotent repeated release.
#[tokio::test]
async fn ui_override_stacking_middle_release_and_self_release() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);

        exec(
            &t.vt.host,
            r#"
            TRACE = {}
            local r1 = mare.ui.override("notify", function(opts, next)
                table.insert(TRACE, "in:1")
                opts.text = "[1]" .. opts.text
                next(opts)
                table.insert(TRACE, "out:1")
            end)
            local r2 = mare.ui.override("notify", function(opts, next)
                table.insert(TRACE, "in:2")
                opts.text = "[2]" .. opts.text
                next(opts)
                table.insert(TRACE, "out:2")
            end)
            local r3 = mare.ui.override("notify", function(opts, next)
                table.insert(TRACE, "in:3")
                opts.text = "[3]" .. opts.text
                next(opts)
                table.insert(TRACE, "out:3")
            end)

            mare.ui.notify("FIRST", "info")
            assert(table.concat(TRACE, ",") == "in:3,in:2,in:1,out:1,out:2,out:3", table.concat(TRACE, ","))

            -- Release middle layer r2 cleanly (twice = idempotent)
            r2()
            r2()
            TRACE = {}
            mare.ui.notify("SECOND", "info")
            assert(table.concat(TRACE, ",") == "in:3,in:1,out:1,out:3", table.concat(TRACE, ","))

            -- Self-releasing top layer delegates to remaining layers and never runs again
            local r_self
            r_self = mare.ui.override("notify", function(opts, next)
                table.insert(TRACE, "in:self")
                r_self()
                opts.text = "[SELF]" .. opts.text
                next(opts)
                table.insert(TRACE, "out:self")
            end)
            TRACE = {}
            mare.ui.notify("THIRD", "info")
            assert(table.concat(TRACE, ",") == "in:self,in:3,in:1,out:1,out:3,out:self", table.concat(TRACE, ","))

            TRACE = {}
            mare.ui.notify("FOURTH", "info")
            assert(table.concat(TRACE, ",") == "in:3,in:1,out:1,out:3", table.concat(TRACE, ","))

            r1()
            r3()
            "#,
        )
        .await;

        t.until("stacked notice outputs reached screen", |s| {
            s.contains("[1][3]FOURTH") || s.contains("[1][2][3]FIRST")
        })
        .await;
    })
    .await
}

/// Passthrough `mare.ui.override("select", ...)` preserves `on_highlight` preview, tabs, search,
/// `on_open` `sel:update()`, `"cancelled"`/`"interrupted"`, and headless `"unavailable"`.
#[tokio::test]
async fn ui_override_select_preserves_tabs_search_preview_update_and_dismissals() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        expose(&t.vt.host);

        exec(
            &t.vt.host,
            r#"
            _G.REL_SEL_PASS = mare.ui.override("select", function(opts, next)
                opts.title = "Pass:" .. tostring(opts.title)
                return next(opts)
            end)

            _G.HIGHLIGHTS = {}
            mare.task.spawn(function()
                R_TAB = { mare.ui.select({
                    title = "Tabbed",
                    tabs = {
                        {
                            name = "AlphaTab",
                            items = {
                                { item_id = "a1", label = "one" },
                            },
                        },
                        {
                            name = "BetaTab",
                            items = {
                                { item_id = "b1", label = "xray" },
                                { item_id = "b2", label = "zebra" },
                            },
                        },
                    },
                    on_highlight = function(idx, tab)
                        table.insert(_G.HIGHLIGHTS, tostring(tab) .. ":" .. tostring(idx))
                        return mare.ui.Line("PREVIEW-" .. tostring(tab) .. "-" .. tostring(idx))
                    end,
                    on_open = function(sel)
                        sel:update({
                            tabs = {
                                {
                                    name = "AlphaTab",
                                    items = {
                                        { item_id = "a1", label = "one" },
                                        { item_id = "a2", label = "two" },
                                    },
                                },
                                {
                                    name = "BetaTab",
                                    items = {
                                        { item_id = "b1", label = "xray" },
                                        { item_id = "b2", label = "zebra" },
                                    },
                                },
                            },
                        })
                    end,
                }) }
            end)
            "#,
        )
        .await;

        t.until("override select open with on_open update + preview", |s| {
            s.contains("Pass:Tabbed") && s.contains("two") && s.contains("PREVIEW-1-1")
        })
        .await;
        assert!(!t.screen().contains("Pass:Pass:"), "unexpected double override:\n{}", t.screen());

        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("switched to BetaTab", |s| s.contains("xray") && s.contains("PREVIEW-2-1")).await;

        t.key(KeyCode::Char('/'), Modifiers::NONE);
        t.vt.type_str("ze");
        t.until("search filtered BetaTab to zebra", |s| s.contains("zebra") && !s.contains("xray")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("tabbed select closed", |s| !s.contains("Pass:Tabbed")).await;
        assert_eq!(
            eval::<(Option<i64>, Option<i64>, Option<String>)>(&t.vt.host, "return R_TAB[1], R_TAB[2], R_TAB[4]").await,
            (Some(2), Some(2), Some("ze".to_string()))
        );
        assert!(eval::<bool>(&t.vt.host, "return #_G.HIGHLIGHTS >= 2").await);

        // Esc -> "cancelled"; Ctrl+C -> "interrupted"
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function() R_ESC = { mare.ui.select({ title = "EscPick", items = { "x" } }) } end)"#,
        )
        .await;
        t.until("EscPick open", |s| s.contains("Pass:EscPick")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("EscPick closed", |s| !s.contains("Pass:EscPick")).await;
        assert_eq!(
            eval::<(Option<i64>, Option<String>)>(&t.vt.host, "return R_ESC[1], R_ESC[2]").await,
            (None, Some("cancelled".to_string()))
        );

        exec(
            &t.vt.host,
            r#"mare.task.spawn(function() R_INT = { mare.ui.select({ title = "IntPick", items = { "x" } }) } end)"#,
        )
        .await;
        t.until("IntPick open", |s| s.contains("Pass:IntPick")).await;
        t.key(KeyCode::Char('c'), Modifiers::CONTROL);
        t.until("IntPick closed", |s| !s.contains("Pass:IntPick")).await;
        assert_eq!(
            eval::<(Option<i64>, Option<String>)>(&t.vt.host, "return R_INT[1], R_INT[2]").await,
            (None, Some("interrupted".to_string()))
        );
    })
    .await;

    // Headless host without UI frontend -> passthrough override preserves `"unavailable"`.
    local(async {
        let env = TestEnv::new().await;
        let host = env.host().user_config(false).faux(json!([])).build().await;
        expose(&host);
        let res: (Option<i64>, Option<String>) = eval(
            &host,
            r#"
            mare.ui.override("select", function(opts, next)
                opts.title = "Headless:" .. tostring(opts.title)
                return next(opts)
            end)
            return mare.ui.select({ title = "NoUI", items = { "a" } })
            "#,
        )
        .await;
        assert_eq!(res, (None, Some("unavailable".to_string())));
        host.shutdown().await;
    })
    .await
}

/// `_wait_hook` (`ui/wait-start` / `ui/wait-end`) and `interaction/requested` / `interaction/resolved`
/// wrap outermost around overrides (never bypassed on short-circuit and never double-emitted on `confirm -> select`).
#[tokio::test]
async fn ui_override_keeps_wait_hook_and_interaction_events_outermost() {
    local(async {
        let env = TestEnv::new().await;
        let host = env.host().user_config(false).faux(json!([])).build().await;
        expose(&host);
        let summary: String = eval(
            &host,
            r#"
            local log = {}
            mare.on("ui/wait-started", function(ev)
                table.insert(log, "ws:" .. ev.kind .. ":" .. tostring(ev.title))
            end)
            mare.on("ui/wait-completed", function(ev)
                table.insert(log, "we:" .. ev.kind .. ":" .. tostring(ev.title))
            end)
            mare.on("interaction/requested", function(ev)
                table.insert(log, "ir:" .. ev.kind .. ":" .. tostring(ev.title))
            end)
            mare.on("interaction/resolved", function(ev)
                table.insert(log, "is:" .. ev.kind .. ":" .. tostring(ev.outcome))
            end)

            -- 1. Short-circuit select override returns 2 -> 1 start/req + answered
            local r1 = mare.ui.override("select", function(opts, _next)
                return 2
            end)
            assert(mare.ui.select({ title = "ShortSel", items = { "a", "b" } }) == 2)

            -- 2. Nested confirm -> select (confirm calls next -> base confirm calls ui.select override!)
            -- Must emit ONLY ONE outer wait/interaction pair with kind="confirm" and outcome="answered" even for false
            local r2 = mare.ui.override("confirm", function(opts, next)
                opts.prompt = "OuterConfirm"
                return next(opts)
            end)
            local ok, err = mare.ui.confirm("InnerConfirm", true)
            assert(ok == false and err == nil, tostring(ok) .. ":" .. tostring(err))
            r1()
            r2()

            -- 3. Cancelled select override -> outcome="cancelled"
            local r3 = mare.ui.override("select", function(_opts, _next)
                return nil, "cancelled"
            end)
            local v, verr = mare.ui.select({ title = "CancelSel", items = { "a" } })
            assert(v == nil and verr == "cancelled")
            r3()

            return table.concat(log, "|")
            "#,
        )
        .await;
        assert_eq!(
            summary,
            concat!(
                "ws:select:ShortSel|ir:select:ShortSel|we:select:ShortSel|is:select:answered|",
                "ws:confirm:InnerConfirm|ir:confirm:InnerConfirm|we:confirm:InnerConfirm|is:confirm:answered|",
                "ws:select:CancelSel|ir:select:CancelSel|we:select:CancelSel|is:select:cancelled"
            )
        );
        host.shutdown().await;
    })
    .await
}

/// Plugin-owned `ctx.ui:override` is withdrawn automatically when the owning plugin is disposed,
/// and direct `init.lua` `mare.ui.override` is replaced/rolled back across `reload_config`.
#[tokio::test]
async fn ui_override_plugin_owner_cleanup_and_init_lua_reload() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        let init_path = env.config_dir().join("init.lua");
        std::fs::write(
            &init_path,
            r#"
            mare.setup { model = "faux/echo" }
            mare.ui.override("select", function(opts, _next)
                return 1, "from-init-v1"
            end)
            "#,
        )
        .unwrap();
        std::fs::write(
            env.config_dir().join("plugins/ov_a.lua"),
            r#"return {
                name = "ov-a",
                inject = { "ui" },
                apply = function(ctx)
                    ctx.ui:register_override("notify", function(opts, next)
                        opts.text = "[A]" .. tostring(opts.text)
                        return next(opts)
                    end)
                end,
            }"#,
        )
        .unwrap();
        std::fs::write(
            env.config_dir().join("plugins/ov_b.lua"),
            r#"return {
                name = "ov-b",
                inject = { "ui" },
                apply = function(ctx)
                    ctx.ui:register_override("notify", function(opts, next)
                        opts.text = "[B]" .. tostring(opts.text)
                        return next(opts)
                    end)
                end,
            }"#,
        )
        .unwrap();

        let host = env.host().user_config(true).no_session().build().await;
        expose(&host);

        exec(
            &host,
            r#"
            _G.SEEN_NOTICES = {}
            mare.ui.notify = function(msg) table.insert(_G.SEEN_NOTICES, tostring(msg)) end
            mare.ui.notify("one", "info")
            "#,
        )
        .await;
        let first_sel: (Option<i64>, Option<String>) =
            eval(&host, "return mare.ui.select({ title = 'S', items = { 'x', 'y' } })").await;
        assert_eq!(first_sel, (Some(1), Some("from-init-v1".to_string())));

        // Dispose ov-a: only ov-b remains on notify.
        unload_plugin(&host.kernel, "ov-a").await;
        exec(&host, "mare.ui.notify('two', 'info')").await;
        let notices: Vec<String> = eval(&host, "return _G.SEEN_NOTICES").await;
        assert_eq!(notices.len(), 2);
        assert!(notices[0].contains("[A]") && notices[0].contains("[B]"), "{notices:?}");
        assert_eq!(notices[1], "[B]two", "disposing ov-a must withdraw only ov-a's override");

        // Reload init.lua with a broken config -> rolls back and preserves v1 override.
        std::fs::write(
            &init_path,
            r#"
            mare.setup { model = "faux/echo" }
            mare.ui.override("select", function() return 99 end)
            error("boom during config reload")
            "#,
        )
        .unwrap();
        assert!(host.reload_config().await.is_err());
        let after_rollback: (Option<i64>, Option<String>) =
            eval(&host, "return mare.ui.select({ title = 'S', items = { 'x', 'y' } })").await;
        assert_eq!(after_rollback, (Some(1), Some("from-init-v1".to_string())));

        // Reload init.lua with v2 override -> replaces v1 cleanly without stacking.
        std::fs::write(
            &init_path,
            r#"
            mare.setup { model = "faux/echo" }
            local seen = {}
            mare.ui.override("select", function(opts, next)
                seen[#seen + 1] = opts.title
                _G.INIT_V2_SEEN = table.concat(seen, ",")
                return 2, "from-init-v2"
            end)
            "#,
        )
        .unwrap();
        host.reload_config().await.unwrap();
        let after_v2: (Option<i64>, Option<String>) =
            eval(&host, "return mare.ui.select({ title = 'Reloaded', items = { 'x', 'y' } })").await;
        assert_eq!(after_v2, (Some(2), Some("from-init-v2".to_string())));

        host.shutdown().await;
    })
    .await
}
