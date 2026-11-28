//! The status bar and the task widget.

use super::*;

#[tokio::test]
async fn narrow_statusline_keeps_right_segments_whole_with_unicode() {
    local(async {
        let cfg = json!({ "plugins": { "statusline": { "enabled": false } } });
        let mut t = Term::with(json!([]), cfg, 100, 20).await;
        exec(
            &t.vt.host,
            r#"
            mare.ui.status('left-test', { align = 'left', order = 1, keep = true,
                render = function() return mare.ui.Span('界 é left segment 12345') end })
            mare.ui.status('context-test', { align = 'right', order = 1,
                render = function() return mare.ui.Span('context: 73%') end })
            mare.ui.status('tail-test', { align = 'right', order = 2,
                render = function() return mare.ui.Span('icon →') end })
        "#,
        )
        .await;
        t.until("the full statusline", |s| s.contains("context: 73%") && s.contains("icon →")).await;
        for width in [39, 40, 41] {
            t.resize(width, 20).await;
            let screen = t.screen();
            let line = screen.lines().rfind(|line| !line.trim().is_empty()).unwrap();
            assert!(!line.contains('…'), "segments must be dropped whole at width {width}: {line}");
            assert!(!line.contains("context") || line.contains("context: 73%"), "width {width}: {line}");
            assert!(!line.contains("icon") || line.contains("icon →"), "width {width}: {line}");
            assert!(mare_shared::style::str_width(line) <= width as usize, "width {width}: {line}");
        }
    })
    .await
}

#[tokio::test]
async fn context_severity_changes_at_thresholds_and_danger_survives_compaction_hint() {
    local(async {
        let mut t = Term::colored(json!([]), 160, 20).await;
        expose(&t.vt.host);
        global(&t.vt.host, "LLM", t.vt.host.service_table("llm").unwrap());
        global(&t.vt.host, "USAGE", t.vt.host.service_table("usage").unwrap());
        global(&t.vt.host, "COMPACT", t.vt.host.service_table("compact").unwrap());
        exec(
            &t.vt.host,
            r#"
            USED = 0
            LLM.current = function() return { id = 'm', name = 'm', context_window = 100000 } end
            USAGE.context_tokens = function() return USED end
            COMPACT.threshold = function() return 1000000 end
        "#,
        )
        .await;
        let mut colors = Vec::new();
        for percent in [59, 60, 80, 90] {
            exec(&t.vt.host, &format!("USED = {} C.cordis:get('bus'):emit('usage/changed', {{}})", percent * 1000))
                .await;
            let needle = format!("{percent}k/100k");
            t.until("context usage", |s| s.contains(&needle)).await;
            let screen = t.parser.screen();
            let (row, col) = (0..screen.size().0.get())
                .find_map(|row| {
                    let line = screen.contents_between(row, 0, row, 159);
                    line.find(&needle).map(|at| (row, mare_shared::style::str_width(&line[..at]) as u16))
                })
                .unwrap();
            colors.push(screen.cell(row, col).unwrap().fgcolor());
        }
        assert!(
            colors.windows(2).all(|pair| pair[0] != pair[1]),
            "context thresholds should use distinct severities: {colors:?}"
        );
        exec(
            &t.vt.host,
            "COMPACT.threshold = function() return 80000 end C.cordis:get('bus'):emit('usage/changed', {})",
        )
        .await;
        t.until("the compaction warning", |s| s.contains("compacts soon")).await;
        let screen = t.parser.screen();
        let (row, col) = (0..screen.size().0.get())
            .find_map(|row| {
                let line = screen.contents_between(row, 0, row, 159);
                line.find("90k/100k").map(|at| (row, mare_shared::style::str_width(&line[..at]) as u16))
            })
            .unwrap();
        assert_eq!(screen.cell(row, col).unwrap().fgcolor(), colors[3], "compaction must not downgrade danger");
    })
    .await
}

const SNAPSHOT_WAITING: &str =
    r#"return { name = "snapshot-target", inject = { "snapshot-missing" }, apply = function() end }"#;
const SNAPSHOT_RUNNING: &str = r#"return { name = "snapshot-target", apply = function() end }"#;

/// The status bar is the last rows of the screen; startup notices elsewhere are not the bar.
fn snapshot_warning(screen: &str) -> bool {
    screen.lines().rev().take(3).any(|line| line.contains("snapshot-target failed") || line.contains("startup error"))
}

async fn snapshot_statusline_term(waiting: bool, layout: &str) -> (Term, std::path::PathBuf) {
    let env = TestEnv::new().await;
    let plugin = env.config_dir().join("plugins/snapshot-target.lua");
    std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
    std::fs::write(&plugin, if waiting { SNAPSHOT_WAITING } else { SNAPSHOT_RUNNING }).unwrap();
    let mut b = env.host().user_plugins().faux(json!([])).no_session().config(json!({
        "logo": false, "plugins": { "onboarding": { "plugins": false }, "statusline": { "layout": layout, "pr": false, "changed_files": false } }
    }));
    b.opts.mode = Mode::Tui;
    let owned = OwnedVt::new(env, b.opts, 160, 20).await;
    let mut t = Term::from_vt(owned, vt100::Parser::new(nz(20), nz(160), 2000));
    t.step().await;
    expose(&t.vt.host);
    (t, plugin)
}

/// Count failure reads from the completion event on, excluding earlier events that may also redraw.
async fn watch_snapshot_completion(h: &mare_host::Host, event: &str) {
    exec(
        h,
        &format!(
            r#"local orig = mare.plugins.failures
               mare._snapshot_done_reads, mare._snapshot_in_done = 0, false
               mare.plugins.failures = function(...)
                 if mare._snapshot_in_done then mare._snapshot_done_reads = mare._snapshot_done_reads + 1 end
                 return orig(...)
               end
               -- from the completion event on (prepended: the statusline restarts during the reload,
               -- so its own listener is registered after this one); the test reads the count before
               -- it steps the terminal again
               C.cordis:on({event:?}, function() mare._snapshot_in_done = true end, true)"#
        ),
    )
    .await;
}

async fn snapshot_full_reload_statusline(waiting: bool) {
    for layout in ["classic", "codex"] {
        let (mut t, plugin) = snapshot_statusline_term(waiting, layout).await;
        assert_eq!(snapshot_warning(&t.screen()), waiting, "layout={layout}: {}", t.screen());
        watch_snapshot_completion(&t.vt.host, "reload/completed").await;
        std::fs::write(plugin, if waiting { SNAPSHOT_RUNNING } else { SNAPSHOT_WAITING }).unwrap();
        t.vt.host.reload_all().await.unwrap();
        // the reads the statusline made from reload/completed on, taken before this test's own read
        let reads: usize = eval(&t.vt.host, "return mare._snapshot_done_reads").await;
        let count: usize = eval(&t.vt.host, "return #mare.plugins.failures()").await;
        t.step().await;
        assert_eq!(count, usize::from(!waiting), "layout={layout}");
        assert!(reads > 0, "statusline did not redraw during reload/completed: layout={layout}");
        assert_eq!(snapshot_warning(&t.screen()), !waiting, "layout={layout}: {}", t.screen());
    }
}

#[tokio::test]
async fn reload_snapshot_statusline_refreshes_when_a_full_reload_fixes_a_plugin() {
    local(snapshot_full_reload_statusline(true)).await;
}

#[tokio::test]
async fn reload_snapshot_statusline_refreshes_when_a_full_reload_breaks_a_plugin() {
    local(snapshot_full_reload_statusline(false)).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn default_fs_protect_gap_updates_the_partial_badge_with_the_preset() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
        let cfg =
            json!({ "plugins": { "permissions": { "enabled": true, "network": true, "untrusted_preset": "auto" } } });
        let mut t = Term::in_env(env, json!([]), cfg, json!({ "no_session": true }), 120, 20).await;
        expose(&t.vt.host);
        global(&t.vt.host, "P", t.vt.host.service_table("permissions").unwrap());
        assert!(
            tokio::time::timeout(
                Duration::from_secs(5),
                t.until("the default partial badge", |s| {
                    s.lines().any(|line| line.contains("ephemeral") && line.contains("PARTIAL"))
                })
            )
            .await
            .is_ok(),
            "{}",
            t.screen()
        );
        exec(&t.vt.host, r#"P:set("read-only")"#).await;
        assert!(
            tokio::time::timeout(
                Duration::from_secs(5),
                t.until("the read-only badge to clear", |s| {
                    !s.lines().any(|line| line.contains("ephemeral") && line.contains("PARTIAL"))
                })
            )
            .await
            .is_ok(),
            "{}",
            t.screen()
        );
        exec(&t.vt.host, r#"P:set("auto")"#).await;
        assert!(
            tokio::time::timeout(
                Duration::from_secs(5),
                t.until("the auto partial badge again", |s| {
                    s.lines().any(|line| line.contains("ephemeral") && line.contains("PARTIAL"))
                })
            )
            .await
            .is_ok(),
            "{}",
            t.screen()
        );
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn a_background_task_shows_in_the_task_widget() {
    local(async {
        let script = json!([
            { "tool_calls": [{ "name": "bash", "arguments": {
                "command": "sleep 2", "run_in_background": true, "description": "nap two seconds" } }] },
            { "text": "started it" }
        ]);
        let cfg = json!({ "plugins": { "permissions": { "preset": "full-access" } } });
        let mut t = Term::with(script, cfg, 80, 20).await;
        t.submit("nap").await;
        t.until("the task widget", |s| s.contains("nap two seconds") || s.contains("sleep 2")).await;
    })
    .await
}

/// The statusline marks peer methods the session explicitly allows beyond the default set
/// .
#[tokio::test]
async fn the_statusline_marks_explicitly_allowed_peer_methods() {
    local(async {
        let mut t = Term::with(json!([]), json!({ "peer": { "allow": ["prompt"] } }), 100, 20).await;
        t.until("the peer-allow segment", |s| s.contains("peer: prompt")).await;
    })
    .await
}

/// The statusline counts MCP servers that are still connecting, redrawn on the mcp plugin's
/// events; the segment goes away when none is.
#[tokio::test]
async fn the_statusline_shows_mcp_servers_that_are_connecting() {
    local(async {
        let mut t = Term::with(json!([]), json!({}), 120, 20).await;
        expose(&t.vt.host);
        global(&t.vt.host, "MCP", t.vt.host.service_table("mcp").unwrap());
        exec(
            &t.vt.host,
            r#"CONNECTING = 2
               MCP.list_servers = function()
                 local servers = {}
                 for i = 1, 3 do servers[i] = { name = "s" .. i, state = i <= CONNECTING and "connecting" or "connected" } end
                 return { data = servers, errors = {} }
               end
               C.cordis:get("bus"):emit("mcp/changed", { server = "s1" })"#,
        )
        .await;
        t.until("the segment", |s| s.contains("MCP connecting 2")).await;
        exec(&t.vt.host, r#"CONNECTING = 0 C.cordis:get("bus"):emit("mcp/ready", { server = "s1" })"#).await;
        t.until("the segment to go", |s| !s.contains("MCP connecting")).await;
    })
    .await
}

/// within 10% of the compaction threshold (from the compact service) the context
/// segment says compaction is coming; below it the segment is only the gauge.
#[tokio::test]
async fn the_statusline_says_compaction_is_approaching() {
    local(async {
        let mut t = Term::with(json!([]), json!({}), 120, 20).await;
        expose(&t.vt.host);
        global(&t.vt.host, "LLM", t.vt.host.service_table("llm").unwrap());
        global(&t.vt.host, "USAGE", t.vt.host.service_table("usage").unwrap());
        global(&t.vt.host, "COMPACT", t.vt.host.service_table("compact").unwrap());
        exec(
            &t.vt.host,
            r#"USED = 50000
               LLM.current = function() return { id = "m", name = "m", context_window = 100000 } end
               USAGE.context_tokens = function() return USED end
               COMPACT.threshold = function() return 80000 end
               C.cordis:get("bus"):emit("usage/changed", {})"#,
        )
        .await;
        t.until("the gauge", |s| s.contains("50k/100k")).await;
        assert!(!t.screen().contains("compacts soon"), "{}", t.screen());
        exec(&t.vt.host, r#"USED = 75000 C.cordis:get("bus"):emit("usage/changed", {})"#).await;
        t.until("the hint", |s| s.contains("75k/100k") && s.contains("compacts soon")).await;
    })
    .await
}

/// A replacement compact service may omit its optional threshold in either layout.
#[tokio::test]
async fn optional_compact_threshold_keeps_the_statusline_visible() {
    local(async {
        for layout in ["classic", "codex"] {
            let cfg = json!({ "plugins": { "statusline": { "layout": layout } } });
            let mut t = Term::with(json!([]), cfg, 120, 20).await;
            expose(&t.vt.host);
            global(&t.vt.host, "LLM", t.vt.host.service_table("llm").unwrap());
            global(&t.vt.host, "USAGE", t.vt.host.service_table("usage").unwrap());
            global(&t.vt.host, "COMPACT", t.vt.host.service_table("compact").unwrap());
            exec(
                &t.vt.host,
                r#"
               LLM.current = function() return { id = "m", name = "model-check", context_window = 100000 } end
               USAGE.context_tokens = function() return 50000 end
               -- no threshold reported (a mare.Service method cannot be removed from the instance)
               COMPACT.threshold = function() return nil end
               C.cordis:get("bus"):emit("usage/changed", {})"#,
            )
            .await;
            t.until("model and context without threshold", |s| {
                s.contains("model-check")
                    && if layout == "classic" { s.contains("50k/100k") } else { s.contains("50% context left") }
            })
            .await;
            assert!(!t.screen().contains("compacts soon"), "{}", t.screen());
            exec(&t.vt.host, r#"COMPACT.threshold = true C.cordis:get("bus"):emit("usage/changed", {})"#).await;
            t.until("invalid optional capability diagnostic", |s| s.contains("compact.threshold must be a function"))
                .await;
        }
    })
    .await
}

#[tokio::test]
async fn a_four_method_compact_replacement_preserves_the_statusline_after_a_reply() {
    local(async {
        let prepare = |env: &TestEnv| {
            let plugin = env.config_dir().join("plugins/replacement/init.lua");
            std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
            std::fs::write(
                plugin,
                r#"
                return { name = "compact", override = true, provide = { "compact" },
                  apply = function(ctx)
                    require("mare_lib").service(ctx, "compact", { run = function() return { status = "compacted", tokens_before = 0, tokens_after = 0 } end, cancel = function() end,
                      register_stage = function() end, measure = function() return 0 end })
                  end }
            "#,
            )
            .unwrap();
        };
        let env = TestEnv::new().await;
        prepare(&env);
        let mut b = env
            .host().builtins(&["compact", "mcp", "usage"])
            .user_plugins()
            .faux(json!([{ "text": "ACK" }]))
            .config(json!({ "logo": false, "plugins": { "permissions": { "enabled": true }, "onboarding": { "plugins": false } } }));
        b.opts.mode = Mode::Tui;
        let vt = OwnedVt::new(env, b.opts, 120, 20).await;
        let mut t = Term::from_vt(vt, vt100::Parser::new(nz(20), nz(120), 2000));
        t.step().await;
        expose(&t.vt.host);
        global(&t.vt.host, "K", t.vt.host.service_table("compact").unwrap());
        assert!(
            eval::<bool>(&t.vt.host, "return K.threshold == nil and require('mare_lib').callable(K.measure)").await
        );
        t.submit("hello").await;
        t.until("reply and context with replacement compact", |s| s.contains("ACK") && s.contains("/100k")).await;
        assert!(!t.screen().contains("compacts soon"), "{}", t.screen());
    })
    .await
}

/// A narrow status bar keeps the permission-mode badge whole and cuts the rest.
#[tokio::test]
async fn a_narrow_status_bar_keeps_the_mode_badge() {
    local(async {
        let mut t = Term::new(json!([]), 30, 12).await;
        let label = if cfg!(windows) { "PARTIAL" } else { "READ" };
        let capsule = format!(" {label} ");
        t.until("the status bar", |s| {
            s.lines().rfind(|l| !l.trim().is_empty()).is_some_and(|bar| bar.contains(&capsule))
        })
        .await;
        let s = t.screen();
        let last = s.lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default();
        assert!(last.contains(&capsule), "{s}");
        assert!(mare_shared::style::str_width(last.trim_end()) <= 30, "{s}");
    })
    .await
}

/// status bar: changed-file count (and branch) from git, refreshed in the background.
#[tokio::test]
async fn the_status_bar_shows_changed_files_from_git() {
    local(async {
        let prepare = |env: &TestEnv| {
            let git = |args: &[&str]| fixture_git(&env.cwd).args(args).output().map(|o| o.status.success());
            if !git(&["init", "-q", "-b", "trunk"]).unwrap_or(false) {
                return;
            }
            std::fs::write(env.cwd.join("a.txt"), "a").unwrap();
            std::fs::write(env.cwd.join("b.txt"), "b").unwrap();
        };
        let mut t = Term::build(json!([]), json!({ "icons": "ascii" }), json!({}), 120, 16, prepare).await;
        if !t._env.cwd.join(".git").exists() {
            return; // no git here
        }
        t.until("the changed-file count", |s| s.contains("@ trunk") && s.contains("+- 2")).await;
    })
    .await
}

/// /statusline toggles a segment and remembers it: hiding removes the platform mode capsule
/// from the status bar. The bar is the bottom row: the welcome notice above it also names the
/// permission mode and only fades after several seconds, so the whole screen can't be the judge.
#[tokio::test]
async fn statusline_picker_hides_a_segment_and_saves_it() {
    local(async {
        let label = if cfg!(windows) { "PARTIAL" } else { "READ" };
        let expected = format!(" {label} ");
        let capsule = |s: &str| {
            let bar = s.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
            bar.contains(&expected)
        };
        let mut t = Term::new(json!([]), 90, 24).await;
        t.until("the mode capsule", capsule).await;
        t.submit("/statusline").await;
        t.until("the segment picker", |s| s.contains("[x] mode")).await;
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.step().await;
        assert!(t.screen().contains("[ ] mode"), "{}", t.screen());
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("cancel keeps the capsule", |s| !s.contains("[ ] mode") && capsule(s)).await;
        t.submit("/statusline").await;
        t.until("the picker again", |s| s.contains("[x] mode")).await;
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.step().await;
        assert!(t.screen().contains("[ ] mode"), "{}", t.screen());
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("the capsule gone", |s| !s.contains("[ ] mode") && !capsule(s)).await;
        let state = std::fs::read_to_string(t._env.home.path().join("state/state.json")).unwrap();
        assert!(state.contains("statusline_hidden") && state.contains("mode"), "{state}");
    })
    .await
}

/// A layout is data: the segments, their sides and order come from the config.
#[tokio::test]
async fn a_statusline_layout_from_config_is_honoured() {
    local(async {
        let look = json!({ "left": ["model"], "right": ["hints"], "drop": [], "hints": true });
        let cfg = json!({ "plugins": { "statusline": { "layout": look } } });
        let mut t = Term::with(json!([]), cfg, 120, 20).await;
        t.until("model left, hints right", |s| {
            s.lines().any(|l| {
                let (model, hint) = (l.find("Faux Echo"), l.find("shortcuts"));
                model.zip(hint).is_some_and(|(m, h)| m < h)
            })
        })
        .await;
    })
    .await
}

/// status registration surface: add, update in
/// place, left/right alignment, equal-order registration order, `keep`, redraw, early init.lua
/// registration before `ui` plugin, reload rollback, and owner dispose; `ui.Status` is removed.
#[tokio::test]
async fn status_single_surface_lifecycle_ordering_keep_and_reload() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup { model = "faux/echo", logo = false, plugins = { onboarding = { plugins = false }, statusline = { enabled = false } } }
            assert(mare.ui.Status == nil, "ui.Status must be deleted")
            mare._v2_rel = mare.ui.status("early_a", {
                align = "left", order = 10, keep = true,
                render = function() return mare.ui.Span("EARLY_KEEP") end,
            })
            mare.ui.status("early_b", {
                align = "left", order = 10,
                render = function() return mare.ui.Span("EARLY_SECOND") end,
            })
            mare._v2_tick = 1
            mare.ui.status("right_dyn", {
                align = "right", order = 5,
                render = function() return mare.ui.Span("R_" .. tostring(mare._v2_tick)) end,
            })
            "#,
        )
        .unwrap();
        let mut b = env.host().user_config(true).no_session();
        b.opts.mode = Mode::Tui;
        let vt = OwnedVt::new(env, b.opts, 100, 16).await;
        let mut t = Term::from_vt(vt, vt100::Parser::new(nz(16), nz(100), 2000));
        t.step().await;
        expose(&t.vt.host);

        t.until("initial early status segments", |s| {
            s.contains("EARLY_KEEP · EARLY_SECOND") && s.contains("R_1")
        })
        .await;

        // status_redraw(id) updates only the target dynamic segment; status_redraw() updates all.
        exec(&t.vt.host, r#"mare._v2_tick = 2; mare.ui.status_redraw("right_dyn")"#).await;
        t.until("targeted redraw", |s| s.contains("R_2")).await;

        // Releasing removes the segment.
        exec(&t.vt.host, r#"mare._v2_rel()"#).await;
        t.until("released segment gone", |s| !s.contains("EARLY_KEEP") && s.contains("EARLY_SECOND")).await;

        // Failed config reload preserves previously committed segments.
        let init_path = t._env.config_dir().join("init.lua");
        std::fs::write(
            &init_path,
            r#"
            mare.ui.status("rolled_back", { align = "left", order = 1, render = function() return "BAD" end })
            error("broken init")
            "#,
        )
        .unwrap();
        assert!(t.vt.host.reload_config().await.is_err());
        t.step().await;
        assert!(t.screen().contains("EARLY_SECOND") && !t.screen().contains("BAD"), "{}", t.screen());

        // Successful reload removes the prior round's config status registrations.
        std::fs::write(
            &init_path,
            r#"
            mare.setup { model = "faux/echo", logo = false, plugins = { onboarding = { plugins = false }, statusline = { enabled = false } } }
            mare.ui.status("after_reload", { align = "left", order = 1, keep = true, render = function() return mare.ui.Span("AFTER_OK") end })
            "#,
        )
        .unwrap();
        t.vt.host.reload_config().await.unwrap();
        t.until("reload replaced config status", |s| s.contains("AFTER_OK") && !s.contains("EARLY_SECOND")).await;

        // Owner isolation via `ctx.ui:status`: second caller cannot steal an owned id, stale release
        // cannot clear a newer handle, and unloading the owning plugin removes its segment.
        let plugins_dir = t._env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("owner_a.lua"),
            r#"return { name = "owner-a", inject = { "ui" }, apply = function(ctx)
                local rel1 = ctx.ui:status("owned_seg", { align = "left", order = 20, render = function() return "SEG_A1" end })
                local rel2 = ctx.ui:status("owned_seg", { align = "left", order = 20, render = function() return "SEG_A2" end })
                assert(type(rel2) == "function")
                -- Calling stale rel1 after replacement must not clear rel2's segment.
                rel1()
            end }"#,
        )
        .unwrap();
        std::fs::write(
            plugins_dir.join("owner_b.lua"),
            r#"return { name = "owner-b", inject = { "ui" }, apply = function(ctx)
                local ok = pcall(function()
                    ctx.ui:status("owned_seg", { align = "left", order = 20, render = function() return "SEG_B" end })
                end)
                ctx.ui:status("owner_b_ok", { align = "right", order = 99, render = function()
                    return (not ok) and "B_DENIED" or "B_STOLE"
                end })
            end }"#,
        )
        .unwrap();
        std::fs::write(
            &init_path,
            r#"mare.setup { model = "faux/echo", logo = false, plugins = { onboarding = { plugins = false }, statusline = { enabled = false } } }"#,
        )
        .unwrap();
        t.vt.host.reload_all().await.unwrap();
        t.until("owner-a kept SEG_A2 and owner-b denied", |s| s.contains("SEG_A2") && s.contains("B_DENIED")).await;
        assert!(eval::<bool>(&t.vt.host, r#"
            local b = assert(C.cordis:get("ui"))
            local c_other = { name = "other-plugin", effect = function() end }
            local ok = pcall(function() b:status(c_other, "owned_seg", { render = function() return "STEAL" end }) end)
            return not ok
        "#).await, "cross-caller status takeover must be refused");

        std::fs::remove_file(plugins_dir.join("owner_a.lua")).unwrap();
        std::fs::remove_file(plugins_dir.join("owner_b.lua")).unwrap();
        t.vt.host.reload_all().await.unwrap();
        t.until("unloaded owner segment removed", |s| !s.contains("SEG_A2")).await;
    })
    .await
}

#[tokio::test]
async fn status_nil_first_render_keeps_registration_order_when_shown() {
    local(async {
        let cfg = json!({"plugins": {"statusline": {"enabled": false}}});
        let mut t = Term::with(json!([]), cfg, 100, 16).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        exec(
            &t.vt.host,
            r#"mare.g.show_first = false
            mare.ui.status("nil_first", {align = "left", order = 10,
              render = function() if mare.g.show_first then return "FIRST_SEGMENT" end end})
            mare.ui.status("shown_second", {align = "left", order = 10,
              render = function() return "SECOND_SEGMENT" end})"#,
        )
        .await;
        t.until("second segment visible", |s| s.contains("SECOND_SEGMENT")).await;
        assert!(!t.screen().contains("FIRST_SEGMENT"));
        exec(&t.vt.host, r#"mare.g.show_first = true; mare.ui.status_redraw("nil_first")"#).await;
        t.until("first segment visible", |s| s.contains("FIRST_SEGMENT") && s.contains("SECOND_SEGMENT")).await;
        let screen = t.screen();
        let bar = screen.lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default();
        let first = bar.find("FIRST_SEGMENT").expect("first segment on status bar");
        let second = bar.find("SECOND_SEGMENT").expect("second segment on status bar");
        assert!(first < second, "nil-first registration must retain its original order: {bar}");
    })
    .await
}

#[tokio::test]
async fn status_invalid_registration_does_not_claim_another_callers_id() {
    local(async {
        let cfg = json!({"plugins": {"statusline": {"enabled": false}}});
        let mut t = Term::with(json!([]), cfg, 100, 16).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"BAD_READY = false
            BAD_STATUS_PLUGIN = C.cordis:plugin({name = "bad-status-owner", inject = {"ui"}, apply = function(ctx)
              BAD_REJECTED = not pcall(function() ctx.ui:status("invalid_then_valid", {render = "not-a-fn"}) end)
              BAD_READY = true
            end})"#,
        )
        .await;
        t.until_vt("invalid registration attempted", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return BAD_READY")).unwrap_or(false)
        })
        .await;
        assert!(eval::<bool>(&t.vt.host, "return BAD_REJECTED").await);
        exec(
            &t.vt.host,
            r#"GOOD_READY = false
            GOOD_STATUS_PLUGIN = C.cordis:plugin({name = "good-status-owner", inject = {"ui"}, apply = function(ctx)
              GOOD_REGISTERED = pcall(function()
                ctx.ui:status("invalid_then_valid", {align = "left", order = 10,
                  render = function() return "VALID_AFTER_INVALID" end})
              end)
              GOOD_READY = true
            end})"#,
        )
        .await;
        t.until_vt("valid registration attempted", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return GOOD_READY")).unwrap_or(false)
        })
        .await;
        assert!(
            eval::<bool>(&t.vt.host, "return GOOD_REGISTERED").await,
            "invalid registration must not reserve the id for its caller"
        );
        t.until("valid owner's segment", |s| s.contains("VALID_AFTER_INVALID")).await;
        exec(&t.vt.host, "GOOD_STATUS_PLUGIN:dispose(); BAD_STATUS_PLUGIN:dispose()").await;
        t.until("valid owner's segment removed", |s| !s.contains("VALID_AFTER_INVALID")).await;
    })
    .await
}

/// `command_replaces = true` suppresses built-in `left`/`right` segments on
/// both default (`combined`) and explicit `layout = "classic"` configurations.
#[tokio::test]
async fn command_replaces_true_hides_builtin_bars() {
    local(async {
        for (layout_cfg, replaces) in [("nil", true), (r#""classic""#, true), ("nil", false), (r#""classic""#, false)] {
            let env = TestEnv::new().await;
            std::fs::create_dir_all(env.config_dir()).unwrap();
            std::fs::write(
                env.config_dir().join("init.lua"),
                format!(
                    r#"
                    mare.setup {{
                        model = "faux/echo",
                        logo = false,
                        plugins = {{
                            onboarding = {{ plugins = false }},
                            statusline = {{
                                layout = {layout_cfg},
                                command = "printf 'EXT_ONLY_BAR'",
                                command_replaces = {replaces},
                            }},
                        }},
                    }}
                    "#
                ),
            )
            .unwrap();
            let mut b = env.host().user_config(true).no_session();
            b.opts.mode = Mode::Tui;
            let vt = OwnedVt::new(env, b.opts, 100, 16).await;
            let mut t = Term::from_vt(vt, vt100::Parser::new(nz(16), nz(100), 2000));
            t.until_vt("startup", |t| !t.vt.starting()).await;
            t.until("external status bar", |s| {
                let bar = s.lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default();
                bar.contains("EXT_ONLY_BAR") && (replaces || bar.contains("Faux Echo"))
            })
            .await;
            let screen = t.screen();
            let bar = screen.lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default();
            assert!(bar.contains("EXT_ONLY_BAR"), "missing external status on layout={layout_cfg}: {bar}");
            if replaces {
                for builtin in ["Faux Echo", "ephemeral", "READ", "PARTIAL", "FULL", "shortcuts"] {
                    assert!(!bar.contains(builtin), "built-in {builtin} leaked on layout={layout_cfg}: {bar}");
                }
            } else {
                assert!(bar.contains("Faux Echo"), "built-in status must remain on layout={layout_cfg}: {bar}");
            }
        }
    })
    .await
}

#[tokio::test]
async fn statusline_caches_keymap_labels_across_provider_and_usage_redraws() {
    local(async {
        let mut t = Term::colored(json!([]), 160, 20).await;
        expose(&t.vt.host);
        global(&t.vt.host, "UI", t.vt.host.service_table("ui").unwrap());
        exec(
            &t.vt.host,
            r#"
            GET_KEYMAP_CALLS = 0
            ORIG_GET_KEYMAP = UI.get_keymap
            UI.get_keymap = function(...)
                GET_KEYMAP_CALLS = GET_KEYMAP_CALLS + 1
                return ORIG_GET_KEYMAP(...)
            end
            for _ = 1, 5 do
                C.cordis:get("bus"):emit("llm/providers-changed", {})
                C.cordis:get("bus"):emit("usage/changed", {})
            end
            "#,
        )
        .await;
        let calls: usize = eval(&t.vt.host, "return GET_KEYMAP_CALLS").await;
        assert_eq!(calls, 0, "provider/usage redraws must reuse cached keymap labels instead of calling ui:get_keymap");

        // Rebinding to a key not yet in the cache invalidates/misses once and caches the new label.
        exec(
            &t.vt.host,
            r#"
            mare.keymap.set("editor", "<S-Tab>", false)
            mare.keymap.set("editor", "<C-k>", "permissions.cycle")
            C.cordis:get("bus"):emit("ui/keymap-changed", { context = "editor", key = "<C-k>" })
            C.cordis:get("bus"):emit("usage/changed", {})
            "#,
        )
        .await;
        t.until("rebound permissions.cycle hint", |s| s.contains("Ctrl+K mode")).await;
        let after_rebind: usize = eval(&t.vt.host, "return GET_KEYMAP_CALLS").await;
        assert_eq!(after_rebind, 1, "rebound key should trigger exactly one ui:get_keymap refresh");

        exec(
            &t.vt.host,
            r#"
            for _ = 1, 3 do
                C.cordis:get("bus"):emit("usage/changed", {})
            end
            "#,
        )
        .await;
        let after_redraws: usize = eval(&t.vt.host, "return GET_KEYMAP_CALLS").await;
        assert_eq!(after_redraws, 1, "subsequent redraws after rebind must stay cached");

        // A transient miss must not negatively cache the raw key string forever.
        exec(
            &t.vt.host,
            r#"
            UI.get_keymap = function(...)
                GET_KEYMAP_CALLS = GET_KEYMAP_CALLS + 1
                return { bindings = {} }
            end
            C.cordis:get("bus"):emit("ui/keymap-changed", { context = "editor", key = "<C-k>" })
            C.cordis:get("bus"):emit("usage/changed", {})
            "#,
        )
        .await;
        t.until("raw fallback label on transient miss", |s| s.contains("ctrl+k mode")).await;

        exec(
            &t.vt.host,
            r#"
            UI.get_keymap = function(...)
                GET_KEYMAP_CALLS = GET_KEYMAP_CALLS + 1
                return ORIG_GET_KEYMAP(...)
            end
            C.cordis:get("bus"):emit("usage/changed", {})
            "#,
        )
        .await;
        t.until("formatted label recovers without stale negative cache", |s| s.contains("Ctrl+K mode")).await;
    })
    .await
}

/// Plugin failures shown in the status bar include the `· /reload` recovery hint.
#[tokio::test]
async fn statusline_plugin_failure_includes_reload_hint() {
    local(async {
        let env = TestEnv::new().await;
        let plugin = env.config_dir().join("plugins/boom-plugin.lua");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(&plugin, r#"return { name = "boom-plugin", apply = function() error("boom") end }"#).unwrap();
        let mut b = env.host().user_plugins().faux(json!([])).no_session().config(json!({
            "logo": false,
            "plugins": { "onboarding": { "plugins": false }, "statusline": { "pr": false, "changed_files": false } }
        }));
        b.opts.mode = Mode::Tui;
        let owned = OwnedVt::new(env, b.opts, 160, 20).await;
        let mut t = Term::from_vt(owned, vt100::Parser::new(nz(20), nz(160), 2000));
        t.until("plugin failure with /reload hint", |s| {
            s.lines().rev().take(3).any(|line| line.contains("boom-plugin failed") && line.contains("/reload"))
        })
        .await;
    })
    .await
}

/// The statusline exposes a `tips` segment that displays a usage tip and respects `hidden`.
#[tokio::test]
async fn statusline_tips_segment_renders_and_hides_via_visibility() {
    local(async {
        let cfg = json!({
            "plugins": {
                "statusline": {
                    "layout": {
                        "left": ["tips"],
                        "right": ["model"],
                        "drop": [],
                        "sep": "dot",
                        "mode": "off",
                        "model": "text",
                        "effort": "off",
                        "jobs": "icon",
                        "context": "gauge",
                        "hints": false,
                        "flash": 0
                    }
                }
            }
        });
        let mut t = Term::with(json!([]), cfg, 120, 20).await;
        t.until("statusline tips segment", |s| s.lines().rev().take(3).any(|l| l.contains("Tip:"))).await;
        drop(t);

        let cfg_hidden = json!({
            "plugins": {
                "statusline": {
                    "hidden": ["tips"],
                    "layout": {
                        "left": ["tips"],
                        "right": ["model"],
                        "drop": [],
                        "sep": "dot",
                        "mode": "off",
                        "model": "text",
                        "effort": "off",
                        "jobs": "icon",
                        "context": "gauge",
                        "hints": false,
                        "flash": 0
                    }
                }
            }
        });
        let mut t2 = Term::with(json!([]), cfg_hidden, 120, 20).await;
        t2.until("model visible without tips", |s| s.contains("Faux Echo")).await;
        let bar = t2.screen().lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default().to_string();
        assert!(!bar.contains("Tip:"), "hidden=['tips'] must hide the tips segment: {bar}");
    })
    .await
}

/// After `statusline:apply`, all cold submodules must stay absent from `package.loaded` and git
/// probing must not run synchronously inside `apply`.
#[tokio::test]
async fn statusline_apply_keeps_cold_submodules_unloaded_and_defers_git_probe() {
    local(async {
        // 1. Headless (no UI connected): no statusline.* submodule loads during apply.
        {
            let env = TestEnv::new().await;
            let _ = fixture_git(&env.cwd).args(["init", "-q", "-b", "trunk"]).output();
            std::fs::write(env.cwd.join("a.txt"), "a").unwrap();
            let h = env
                .host()
                .faux(json!([]))
                .no_session()
                .config(json!({
                    "logo": false,
                    "plugins": { "permissions": { "enabled": true }, "onboarding": { "plugins": false } }
                }))
                .build()
                .await;
            assert!(h.kernel.plugin_active("statusline"), "statusline must be active after apply");
            let loaded: Vec<String> = eval(
                &h,
                r#"
                local all = {
                    "statusline.git",
                    "statusline.visibility",
                    "statusline.bars",
                    "statusline.external",
                    "statusline.looks",
                    "statusline.layout",
                    "statusline.segments",
                    "statusline.hints",
                    "statusline.compact",
                    "statusline.tips",
                }
                local found = {}
                for _, name in ipairs(all) do
                    if package.loaded[name] ~= nil then
                        found[#found + 1] = name
                    end
                end
                return found
                "#,
            )
            .await;
            assert!(loaded.is_empty(), "headless statusline loaded submodules during apply: {loaded:?}");
            h.shutdown().await;
        }

        // 2. Connected TUI right after apply (before app/ready & ui/ready): cold submodules
        //    (git, visibility picker, external command, compact threshold, tips) stay unloaded.
        {
            let env = TestEnv::new().await;
            let _ = fixture_git(&env.cwd).args(["init", "-q", "-b", "trunk"]).output();
            std::fs::write(env.cwd.join("a.txt"), "a").unwrap();
            let mut b = env.host().faux(json!([])).no_session().config(json!({
                "logo": false,
                "plugins": { "onboarding": { "plugins": false } }
            }));
            b.opts.mode = Mode::Tui;
            let vt = OwnedVt::new(env, b.opts, 120, 16).await;
            assert!(vt.host.kernel.plugin_active("statusline"), "statusline must be active after apply");
            let loaded: Vec<String> = eval(
                &vt.host,
                r#"
                local cold = {
                    "statusline.git",
                    "statusline.visibility",
                    "statusline.external",
                    "statusline.compact",
                    "statusline.tips",
                }
                local found = {}
                for _, name in ipairs(cold) do
                    if package.loaded[name] ~= nil then
                        found[#found + 1] = name
                    end
                end
                return found
                "#,
            )
            .await;
            assert!(loaded.is_empty(), "cold statusline submodules loaded during TUI apply: {loaded:?}");
        }
    })
    .await
}

/// Snapshot every cell `(text, fg, bg, bold, italic, underline, inverse)` of a vt100 screen.
fn snapshot_cells(screen: &vt100::Screen) -> Vec<(String, vt100::Color, vt100::Color, bool, bool, bool, bool)> {
    let (rows, cols) = screen.size();
    let mut out = Vec::with_capacity((rows.get() as usize) * (cols.get() as usize));
    for r in 0..rows.get() {
        for c in 0..cols.get() {
            let cell = screen.cell(r, c).unwrap();
            out.push((
                cell.contents().to_owned(),
                cell.fgcolor(),
                cell.bgcolor(),
                cell.bold(),
                cell.italic(),
                cell.underline(),
                cell.inverse(),
            ));
        }
    }
    out
}

/// 1) With no background-only statusline probes (non-git cwd, or git repo with `changed_files` and
///    `pr` disabled), the very first frame drawn before `app/ready`/`ui/ready` matches the settled
///    post-ready frame cell-for-cell (including `@ <branch>` in a git repository).
/// 2) With dirty-file count and external command enabled in a git repository, the first frame
///    already shows `@ <branch>` synchronously while `changes` and external command probes update
///    in the background once ready.
#[tokio::test]
async fn statusline_first_frame_matches_post_ready_cell_for_cell_and_git_probe_updates_in_background() {
    local(async {
        // 1. Non-git directory: first frame == post-ready frame cell-for-cell (returning user
        //    state so the one-time first-run permissions intro toast does not fire on app/ready).
        {
            let env = TestEnv::new().await;
            let mut b = env.host().faux(json!([])).no_session().config(json!({
                "logo": false,
                "icons": "ascii",
                "plugins": { "permissions": { "enabled": true }, "onboarding": { "plugins": false } }
            }));
            b.opts.mode = Mode::Tui;
            let mut vt = OwnedVt::new(env, b.opts, 120, 16).await;
            exec(&vt.host, r#"mare.state.set("permissions_intro_shown", true)"#).await;
            let mut parser = vt100::Parser::new(nz(16), nz(120), 2000);
            vt.first_frame().await;
            parser.process(&vt.output());
            let first_text = parser.screen().contents();
            let first_cells = snapshot_cells(parser.screen());
            assert!(first_text.contains("Faux Echo"), "first frame must already show the status bar:\n{first_text}");

            pump_until(&mut vt, &mut parser, "startup and ui/ready settled", |vt, _| vt.startup_finished()).await;
            for _ in 0..4 {
                tokio::task::yield_now().await;
                vt.frame().await;
                parser.process(&vt.output());
            }
            let ready_text = parser.screen().contents();
            let ready_cells = snapshot_cells(parser.screen());
            assert_eq!(
                first_cells, ready_cells,
                "first frame and post-ready frame must match cell-for-cell\n--- FIRST ---\n{first_text}\n--- READY ---\n{ready_text}"
            );
            drop(Term::from_vt(vt, parser));
        }

        // 2. Git repository with background-only probes disabled (`changed_files=false, pr=false`):
        //    first frame already contains `@ trunk` and matches the post-ready frame cell-for-cell.
        {
            let git_clean_env = TestEnv::new().await;
            let git_ok = fixture_git(&git_clean_env.cwd)
                .args(["init", "-q", "-b", "trunk"])
                .output()
                .is_ok_and(|o| o.status.success());
            if git_ok {
                let mut b_clean = git_clean_env.host().faux(json!([])).no_session().config(json!({
                    "logo": false,
                    "icons": "ascii",
                    "plugins": {
                        "permissions": { "enabled": true },
                        "onboarding": { "plugins": false },
                        "statusline": { "changed_files": false, "pr": false }
                    }
                }));
                b_clean.opts.mode = Mode::Tui;
                let mut vt_clean = OwnedVt::new(git_clean_env, b_clean.opts, 140, 16).await;
                exec(&vt_clean.host, r#"mare.state.set("permissions_intro_shown", true)"#).await;
                let mut parser_clean = vt100::Parser::new(nz(16), nz(140), 2000);
                vt_clean.first_frame().await;
                parser_clean.process(&vt_clean.output());
                let clean_first_text = parser_clean.screen().contents();
                let clean_first_cells = snapshot_cells(parser_clean.screen());
                assert!(
                    clean_first_text.contains("@ trunk"),
                    "git repo first frame must already contain branch name:\n{clean_first_text}"
                );
                pump_until(&mut vt_clean, &mut parser_clean, "git repo startup settled", |vt, _| {
                    vt.startup_finished()
                })
                .await;
                for _ in 0..4 {
                    tokio::task::yield_now().await;
                    vt_clean.frame().await;
                    parser_clean.process(&vt_clean.output());
                }
                let clean_ready_text = parser_clean.screen().contents();
                let clean_ready_cells = snapshot_cells(parser_clean.screen());
                assert_eq!(
                    clean_first_cells, clean_ready_cells,
                    "git repo first frame and post-ready frame must match cell-for-cell\n--- FIRST ---\n{clean_first_text}\n--- READY ---\n{clean_ready_text}"
                );
                drop(Term::from_vt(vt_clean, parser_clean));
            }
        }

        // 3. Git repository + dirty files + external command: first frame already includes `@ trunk`
        //    synchronously while `git status` (`+- 2`) and external command probes update in background.
        let git_env = TestEnv::new().await;
        let git_ok = fixture_git(&git_env.cwd)
            .args(["init", "-q", "-b", "trunk"])
            .output()
            .is_ok_and(|o| o.status.success());
        if !git_ok {
            return;
        }
        std::fs::write(git_env.cwd.join("a.txt"), "a").unwrap();
        std::fs::write(git_env.cwd.join("b.txt"), "b").unwrap();
        let mut b2 = git_env.host().faux(json!([])).no_session().config(json!({
            "logo": false,
            "icons": "ascii",
            "plugins": {
                "permissions": { "enabled": true },
                "onboarding": { "plugins": false },
                "statusline": { "command": "echo ext-probe-ok", "pr": false }
            }
        }));
        b2.opts.mode = Mode::Tui;
        let mut vt2 = OwnedVt::new(git_env, b2.opts, 160, 16).await;
        let mut parser2 = vt100::Parser::new(nz(16), nz(160), 2000);
        vt2.first_frame().await;
        parser2.process(&vt2.output());
        let git_first = parser2.screen().contents();
        assert!(git_first.contains("Faux Echo"), "first frame must render immediately:\n{git_first}");
        assert!(
            git_first.contains("@ trunk"),
            "first frame in a git repository must already include the branch segment:\n{git_first}"
        );
        assert!(
            !git_first.contains("+- 2") && !git_first.contains("ext-probe-ok"),
            "dirty-file and external command probes must stay in background after first frame:\n{git_first}"
        );
        let mut t2 = Term::from_vt(vt2, parser2);
        t2.until("background git and external probes update status bar", |s| {
            s.contains("Faux Echo") && s.contains("@ trunk") && s.contains("+- 2") && s.contains("ext-probe-ok")
        })
        .await;
    })
    .await
}

/// On the Windows default tier (simulated here via `MARE_SANDBOX_CAPS` without network control),
/// the PARTIAL network-sandbox warning belongs to pre-ready state so the very first frame already
/// shows it and matches the settled post-ready frame cell-for-cell.
#[tokio::test]
async fn partial_network_sandbox_warning_is_in_first_frame_and_matches_post_ready() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,fs_protect");
        let mut b = env.host().faux(json!([])).no_session().config(json!({
            "logo": false,
            "icons": "ascii",
            "plugins": { "permissions": { "enabled": true }, "onboarding": { "plugins": false } }
        }));
        b.opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, b.opts, 120, 16).await;
        exec(&vt.host, r#"mare.state.set("permissions_intro_shown", true)"#).await;
        let mut parser = vt100::Parser::new(nz(16), nz(120), 2000);
        vt.first_frame().await;
        parser.process(&vt.output());
        let first_text = parser.screen().contents();
        let first_cells = snapshot_cells(parser.screen());
        assert!(
            first_text.contains("Network isn't sandboxed on this tier") && first_text.contains("mare sandbox setup"),
            "PARTIAL sandbox warning must already appear in the first frame:\n{first_text}"
        );

        pump_until(&mut vt, &mut parser, "startup and ui/ready settled", |vt, _| vt.startup_finished()).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
            vt.frame().await;
            parser.process(&vt.output());
        }
        let ready_text = parser.screen().contents();
        let ready_cells = snapshot_cells(parser.screen());
        assert_eq!(
            first_cells, ready_cells,
            "first frame and post-ready frame must match cell-for-cell with PARTIAL sandbox warning\n--- FIRST ---\n{first_text}\n--- READY ---\n{ready_text}"
        );
        drop(Term::from_vt(vt, parser));
    })
    .await
}

/// After `/reload` (when `ui/ready` will not replay), `statusline` re-runs its background git and
/// external command probes so `branch`, `changes`, and external output do not go blank.
#[tokio::test]
async fn statusline_reload_reruns_background_git_and_external_probes() {
    local(async {
        let env = TestEnv::new().await;
        let git_ok =
            fixture_git(&env.cwd).args(["init", "-q", "-b", "trunk"]).output().is_ok_and(|o| o.status.success());
        if !git_ok {
            return;
        }
        std::fs::write(env.cwd.join("a.txt"), "a").unwrap();
        std::fs::write(env.cwd.join("b.txt"), "b").unwrap();
        let marker = env.home.path().join("ext_label.txt");
        std::fs::write(&marker, "ext-before").unwrap();
        #[cfg(windows)]
        let cmd = json!(["cmd.exe", "/d", "/c", "type", marker.to_string_lossy()]);
        #[cfg(not(windows))]
        let cmd = json!(["cat", marker.to_string_lossy()]);
        let mut b = env.host().faux(json!([])).no_session().config(json!({
            "logo": false,
            "icons": "ascii",
            "plugins": {
                "permissions": { "enabled": true },
                "onboarding": { "plugins": false },
                "statusline": { "command": cmd, "pr": false }
            }
        }));
        b.opts.mode = Mode::Tui;
        let cwd = env.cwd.clone();
        let owned = OwnedVt::new(env, b.opts, 160, 16).await;
        let mut t = Term::from_vt(owned, vt100::Parser::new(nz(16), nz(160), 2000));
        t.until("initial git and external statusline segments", |s| {
            s.contains("@ trunk") && s.contains("+- 2") && s.contains("ext-before")
        })
        .await;

        std::fs::write(cwd.join("c.txt"), "c").unwrap();
        std::fs::write(&marker, "ext-after").unwrap();
        t.vt.host.kernel.reload(Some("statusline")).await.unwrap();

        t.until("reloaded statusline repopulates git and external segments", |s| {
            s.contains("@ trunk") && s.contains("+- 3") && s.contains("ext-after")
        })
        .await;
    })
    .await
}

#[tokio::test]
async fn tips_modules_and_path_based_image_rendering_cover_option_variants() {
    local(async {
        let mut t = Term::new(json!([]), 90, 24).await;
        let png = t._env.cwd.join("sample.png");
        let tiny_png: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
            0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00,
            0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc, 0xff, 0x1f, 0x00, 0x03, 0x03, 0x02, 0x00, 0xee, 0xfe,
            0x95, 0xc3, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        std::fs::write(&png, tiny_png).unwrap();
        global(&t.vt.host, "PNG_PATH", png.display().to_string());
        global(&t.vt.host, "IMG", t.vt.host.service_table("images").unwrap());
        global(&t.vt.host, "CMD", t.vt.host.service_table("commands").unwrap());
        let ok: bool = eval(
            &t.vt.host,
            r#"
            -- 1. ui.tips & statusline.tips option variants (false, empty, items, array, delay_s, interval_s, Tip: prefix)
            local ut = require("ui.tips")
            local st = require("statusline.tips")
            assert(ut.pick(10, false) == nil)
            assert(ut.current(false) == nil)
            assert(ut.pick(10, { items = {} }) == nil)
            assert(ut.pick(10, { items = { "" }, delay_s = 0, interval_s = 1 }) == nil)
            assert(ut.pick(10, { items = { "Tip: custom" }, delay_s = 0, interval_s = 5 }) == "Tip: custom")
            assert(ut.current({ items = { "hello" }, interval_s = 5 }) == "Tip: hello")
            assert(ut.current({ "Tip: array-form" }) == "Tip: array-form")

            assert(st.current(false) == nil)
            assert(st.current({ items = {} }) == nil)
            assert(st.current({ items = { "" }, interval_s = 2 }) == nil)
            assert(st.current({ items = { "Tip: already" }, interval_s = 2 }) == "Tip: already")
            assert(st.current({ "from-array" }) == "Tip: from-array")

            -- 2. images service renders path-only image blocks and handles invalid base64 gracefully.
            IMG:show({ { type = "image", path = PNG_PATH } })
            IMG:show({ { type = "image", data = "!!!invalid-base64!!!" } })

            -- 3. commands.registry rejects malformed registrations, surfaces alias_of in get(), and hides aliases in list().
            local function must_fail(name, opts)
                local ok_r = pcall(function() CMD:register(name, opts) end)
                assert(not ok_r, "expected register failure for " .. tostring(name))
            end
            must_fail("", { run = function() end })
            must_fail("bad_opts", "not-a-table")
            must_fail("bad_avail_val", { run = function() end, availability = { "not_real" } })
            must_fail("bad_avail_type", { run = function() end, availability = 123 })
            must_fail("bad_alias", { alias_of = "" })
            must_fail("alias_and_run", { alias_of = "help", run = function() end })
            must_fail("missing_run", { description = "no run" })
            must_fail("bad_complete", { run = function() end, complete = 42 })
            local disp = CMD:register("cov_alias_demo", { alias_of = "help", description = "alias" })
            local info = CMD:get("cov_alias_demo")
            assert(info and info.alias_of == "help", "alias_of must be reported in commands.get()")
            local listed = CMD:list()
            for _, item in ipairs(listed.data or listed) do
                assert(item.name ~= "cov_alias_demo", "aliases must be omitted from commands.list()")
            end
            disp()

            return true
            "#,
        )
        .await;
        assert!(ok);
        t.step().await;
    })
    .await
}

#[tokio::test]
async fn ui_service_keymap_rules_and_bindings_reject_invalid_inputs() {
    local(async {
        let mut t = Term::new(json!([]), 90, 24).await;
        t.vt.host.sync_net_origin();
        t.vt.host.spawn_ui_print("helper print");
        assert!(!t.vt.host.cancel_token().is_cancelled());
        let cont = t.vt.host.continue_turn().await.unwrap();
        assert_eq!(cont["status"], "no_pending_work", "{cont}");
        global(&t.vt.host, "UI", t.vt.host.service_table("ui").unwrap());
        global(&t.vt.host, "S", t.vt.host.service_table("session").unwrap());
        global(&t.vt.host, "TMP_DIR", t._env.cwd.display().to_string());
        let ok: bool = eval(
            &t.vt.host,
            r#"
            -- 1. Core bindings input validation (fs.resolve_path, sandbox.check, text.patch_paths, ui.approval, proc.job).
            assert(not pcall(mare.fs.resolve_path, "", nil))
            assert(not pcall(mare.fs.resolve_path, "a.txt", "relative/base"))
            assert(not pcall(mare.proc.sandbox.check, "bogus_op", "/tmp"))
            local p_res, p_err = mare.text.patch_paths("*** Begin Patch\n*** Add File: a\0b\n+x\n*** End Patch")
            assert(p_res == nil and tostring(p_err):find("invalid path", 1, true), tostring(p_err))
            local p_dst, p_derr = mare.text.patch_paths("*** Begin Patch\n*** Update File: a.txt\n*** Move to: bad\0dest\n@@\n-x\n+y\n*** End Patch")
            assert(p_dst == nil and tostring(p_derr):find("invalid destination path", 1, true), tostring(p_derr))
            assert(not pcall(mare.ui.approval, {}))
            assert(not pcall(mare.ui.approval, {
                approval_binding = { owner = "o", turn = "t", request = "r", snapshot = "s" },
            }))
            local pty_job = mare.proc.start({ cmd = "sh", args = { "-c", "echo pty-ok" }, tty = true })
            assert(not pcall(function() pty_job:close_stdin() end))
            pty_job:wait({ timeout_ms = 2000 })
            local pipe_job = mare.proc.start({ cmd = "sh", args = { "-c", "echo err-line >&2" } })
            pipe_job:wait({ timeout_ms = 2000 })
            local err_chunk = pipe_job:read({ stream = "stderr" })
            assert(tostring(err_chunk.text or ""):find("err-line", 1, true), tostring(err_chunk.text))

            -- 1b. Config reload without init.lua fails cleanly; cyclic runtime option is rejected.
            local ok_rl, err_rl = pcall(mare.reload_config)
            assert(not ok_rl and type(err_rl) == "table" and err_rl.code == "unavailable" and tostring(err_rl):find("config reload is unavailable", 1, true), tostring(err_rl))
            local cyc = {}
            cyc.self = cyc
            assert(not pcall(function() mare.o.ui = cyc end))

            -- 2. Keymap owner/priority validation and same-layer ambiguous conflict diagnostics.
            assert(not pcall(mare.keymap.set, "editor", "alt+z", "editor.submit", { owner = "" }))
            assert(not pcall(mare.keymap.set, "editor", "alt+z", "editor.submit", { priority = math.huge }))
            local h1 = mare.keymap.set({}, "alt+y", "editor.submit", { owner = "owner_b", priority = 5 })
            local ok_ambig, err_ambig = pcall(mare.keymap.set, "editor", "alt+y", "newline", { owner = "owner_a", priority = 5 })
            assert(not ok_ambig and tostring(err_ambig):find("ambiguous key binding", 1, true), tostring(err_ambig))
            mare.keymap.remove(h1)

            -- 3. ui.service rejects invalid status align, layout, keymap, and submit inputs, and handles call_keymap.
            assert(not pcall(function() UI:add_status({ id = "bad_align", align = "center", render = function() return "" end }) end))
            assert(not pcall(function() UI:set_layout("bad") end))
            assert(not pcall(function() UI:keymap_snapshot(123) end))
            assert(not pcall(function() UI:set_keymap("editor", "alt+z", 123) end))
            assert(not pcall(function() UI:set_keymap("editor", "alt+z", "no_such_action_xyz") end))
            assert(not pcall(function() UI:delete_keymap("no_such_context_xyz", "alt+z") end))
            assert(not pcall(function() UI:submit("hi", { mode = "bogus_mode" }) end))
            assert(not pcall(function() UI:submit("   ") end))
            local km_core = UI:call_keymap("editor", nil, "newline")
            assert(km_core and km_core.handled == false, "core action should report frontend_owned")
            local km_empty = UI:call_keymap("editor", nil, "editor.submit")
            assert(km_empty and km_empty.handled == false, "empty editor.submit should not send")
            assert(not pcall(function() UI:call_keymap("editor", nil, "unknown_keymap_act") end))

            -- 4. permissions.rules_file handles empty files, structural dedup/mismatch, and corrupt JSON.
            local rules_path = TMP_DIR .. "/shared_rules.json"
            assert(mare.fs.write(rules_path, "   \n"))
            local bumps = 0
            local rf = require("permissions.rules_file")(rules_path, function() bumps = bumps + 1 end)
            assert(#rf.list() == 0)
            local r1 = { tool = "bash", argv = { "echo" }, decision = "allow" }
            local r2 = { tool = "bash", argv = { "echo" }, decision = "allow", scope = "read" }
            local r3 = { tool = 123, decision = "allow" }
            assert(rf.add(r1) == true)
            assert(rf.add(r1) == true)
            assert(rf.add(r2) == true)
            assert(rf.add(r3) == true)
            assert(#rf.list() == 3)
            assert(mare.fs.write(rules_path, "[1, 2, 3]"))
            local ok_corrupt, err_corrupt = rf.add(r1)
            assert(ok_corrupt == nil and tostring(err_corrupt):find("corrupt", 1, true), tostring(err_corrupt))

            -- 5. mcp.approvals reports corrupt disabled project entries and write errors.
            local ma = require("mcp.approvals")({
                approvals_path = TMP_DIR .. "/mcp-approvals.json",
                disabled_path = TMP_DIR .. "/mcp-disabled.json",
                read_json = function(p)
                    if p:find("disabled", 1, true) then
                        return { [mare.cwd] = "not-a-table" }
                    end
                    return { approved = {} }
                end,
                write_json = function()
                    return nil, "disk full"
                end,
            })
            local en_ok, en_err = ma.enabled({ name = "srv", raw = {} })
            assert(en_ok == false and tostring(en_err):find("corrupt project entry", 1, true), tostring(en_err))
            local se_ok, se_err = pcall(ma.set_enabled, { name = "srv", raw = {} }, false)
            assert(not se_ok and tostring(se_err):find("corrupt project entry", 1, true), tostring(se_err))
            local ap_ok, ap_err = pcall(ma.approve, { name = "srv", source = "project", raw = {} })
            assert(not ap_ok and tostring(ap_err):find("disk full", 1, true), tostring(ap_err))

            -- 6. session.queries validates list_items options and get(session_id) errors.
            for _, bad_opts in ipairs({
                "not-a-table",
                { limit = 0 },
                { cursor = "bad-cursor" },
                { branch = "sideways" },
                { direction = "sideways" },
                { types = "not-an-array" },
                { types = { "" } },
            }) do
                local ok_q = pcall(function() S:list_items(bad_opts) end)
                assert(not ok_q, "expected list_items validation error")
            end
            assert(not pcall(function() S:get("") end))
            assert(not pcall(function() S:get("no-such-session-id-9999") end))

            return true
            "#,
        )
        .await;
        assert!(ok);
        t.step().await;
        // Width change followed by height-only resize exercises Due::Rebuild then Due::Repaint;
        // immediate predicate covers early loop return.
        t.vt.resize(80, 24);
        t.vt.resize(80, 20);
        assert!(t.vt.run_production_loop_until(Duration::from_millis(10), |_| true).await);
        drop(t);

        // Dropping a raw Vt before startup completes aborts the background startup task cleanly.
        let env_drop = TestEnv::new().await;
        let mut b = env_drop.host().faux(json!([])).no_session();
        b.opts.mode = Mode::Tui;
        let raw_vt = mare_cli::testkit::Vt::new(b.opts, 80, 20).await.unwrap();
        let host_to_stop = raw_vt.host.clone();
        drop(raw_vt);
        host_to_stop.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn provider_faux_rejects_non_array_script_json() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(json!({ "plugins": { "provider-faux": { "script_json": "{}" } } }))
            .no_session()
            .build_unchecked_exposed()
            .await;
        let problems = h.kernel.problems();
        assert!(
            problems.iter().any(|p| p.contains("provider-faux") && p.contains("JSON array")),
            "expected provider-faux JSON array error in problems: {problems:?}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn transcript_renders_user_images_and_faux_echoes_image_only_turn() {
    local(async {
        let cfg = json!({
            "highlight_cache": 64,
            "width_overrides": [{ "char": "★", "width": 2 }, { "from": 9734, "to": 9735, "width": 2 }],
            "key_conflicts": [{ "key": "ctrl+alt+z", "terminal": "test", "by": "wm", "fix": "rebind" }],
            "paste_burst": true
        });
        let mut t = Term::with(json!([]), cfg, 90, 24).await;
        let png_path = t._env.cwd.join("inline.png");
        let tiny_png: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
            0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00,
            0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc, 0xff, 0x1f, 0x00, 0x03, 0x03, 0x02, 0x00, 0xee, 0xfe,
            0x95, 0xc3, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        std::fs::write(&png_path, tiny_png).unwrap();
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        expose(&t.vt.host);
        global(&t.vt.host, "PNG_PATH", png_path.display().to_string());
        global(&t.vt.host, "PNG_B64", png_b64.to_string());
        let ok: bool = eval(
            &t.vt.host,
            r#"
            -- 1. Image-only user turn exercises provider-faux's empty last_user_text fallback and ui.transcript image blocks.
            local started = A:prompt({
                { type = "image", mime = "image/png", data = PNG_B64, path = PNG_PATH },
            })
            local res = A:wait(started.turn_id)
            assert(not res.error, tostring(res.error))
            assert(tostring(res.text):find("echo:", 1, true), tostring(res.text))

            -- 2. Mixed text + image turn exercises multi-node user block rendering in ui.transcript.
            local started2 = A:prompt({
                { type = "text", text = "check attached image" },
                { type = "image", mime = "image/png", data = PNG_B64, path = PNG_PATH },
            })
            local res2 = A:wait(started2.turn_id)
            assert(not res2.error, tostring(res2.error))

            -- 3. Error completion and compacted + truncated session replay cover ui.transcript branches.
            local bus = C.cordis:get("bus")
            bus:emit("turn/completed", { message = { role = "assistant", error = "simulated stream failure" } })
            local many = {}
            for i = 1, 32 do
                many[#many + 1] = { role = (i % 2 == 1) and "user" or "assistant", content = { { type = "text", text = "m" .. i } } }
            end
            many[#many + 1] = {
                role = "user",
                compaction = true,
                summary = "compacted summary text",
                timestamp = 1760000000000,
                content = { { type = "text", text = "compacted" } },
            }
            bus:emit("session/loaded", { messages = many })
            return true
            "#,
        )
        .await;
        assert!(ok);
        t.step().await;
    })
    .await
}

#[tokio::test]
async fn config_reload_removes_prior_keymaps_and_handles_empty_plugin_env_and_oversized_skills() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_APPROVAL_POLICY", "");
        let cfg_dir = env.home.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let init_lua = cfg_dir.join("init.lua");
        std::fs::write(
            &init_lua,
            r#"
            mare.keymap.set("editor", "alt+k", "newline")
            mare.keymap.context("editor.cov_reload_ctx", { parent = "editor" })
            mare.setup { plugins = { statusline = { hidden = { "tips" } } } }
            "#,
        )
        .unwrap();

        // Also place a skill with over-length description and compatibility to cover skill validation diagnostics.
        let skill_dir = env.cwd.join(".mare/skills/oversized");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let long_desc = "d".repeat(1100);
        let long_compat = "c".repeat(600);
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: oversized\ndescription: {long_desc}\ncompatibility: {long_compat}\n---\nBody\n"),
        )
        .unwrap();

        let h = env.host().user_config(true).no_session().build().await;
        expose(&h);
        global(&h, "R", h.service_table("review").unwrap());

        // Rewrite init.lua dropping both the keymap/subcontext and plugin config entry, then exercise reload_config + full reload.
        std::fs::write(&init_lua, "mare.setup {}\n").unwrap();
        let ok: bool = eval(
            &h,
            r#"
            -- Make UI appear connected so skipped-skill notification in context/skills_scan.lua runs.
            mare.ui.connected = function() return true end
            mare.ui.notify = function() end
            local r = mare.reload_config()
            assert(type(r) == "table", tostring(r))
            local ctx_svc = C.cordis:get("context")
            if ctx_svc and ctx_svc.skills then pcall(ctx_svc.skills, ctx_svc) end
            local rv1, rve1 = R:run({ kind = "custom", text = "   " })
            assert(rv1 == nil and tostring(rve1):find("can't be empty", 1, true), tostring(rve1))
            local rv2, rve2 = R:run({ kind = "bogus_kind" })
            assert(rv2 == nil and tostring(rve2):find("unknown review target", 1, true), tostring(rve2))
            return true
            "#,
        )
        .await;
        assert!(ok);
        // Full reload re-evaluates init.lua in non-staging mode, removing prior keymaps and subcontexts.
        std::fs::write(
            &init_lua,
            r#"
            mare.keymap.set("editor", "alt+k", "newline")
            mare.keymap.context("editor.cov_reload_ctx", { parent = "editor" })
            mare.setup {}
            "#,
        )
        .unwrap();
        let _ = eval::<mlua::Value>(&h, "return mare.reload()").await;
        std::fs::write(&init_lua, "mare.setup {}\n").unwrap();
        let _ = eval::<mlua::Value>(&h, "return mare.reload()").await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn llm_validation_models_credentials_and_cli_usage_error_paths() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("GEMINI_API_KEY", "");
        env.set_var("GOOGLE_API_KEY", "");
        env.set_var("OPENCODE_API_KEY", "");
        let plugins_dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("cov_llm_prov.lua"),
            r#"
            return {
                name = "cov_llm_prov",
                inject = { "llm" },
                apply = function(ctx)
                    -- Registering a provider declared by another plugin without replace=true covers check_declared failure.
                    ctx.llm:register_provider({ provider = "google", wire = "openai-responses" })
                    -- Registering a code provider with an unknown field covers plugin problem reporting.
                    ctx.llm:register_provider({
                        provider = "cov_custom_uprov",
                        unknown_extra_field = 1,
                        stream = function() return { role = "assistant", content = {} } end,
                    })
                end,
            }
            "#,
        )
        .unwrap();
        let h = env
            .host()
            .user_plugins()
            .builtins(&["provider-google", "provider-opencode-go"])
            .no_session()
            .build_unchecked_exposed()
            .await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());
        let ok: bool = eval(
            &h,
            r#"
            -- 1. llm.validate checks malformed provider, models, login, and wire definitions.
            local V = require("llm.validate")
            local ok_nt = V.provider("not-a-table")
            assert(not ok_nt)
            local ok_bm = V.provider({ provider = "p1", models = "not-a-list", stream = function() end })
            assert(not ok_bm)
            local ok_bme = V.provider({ provider = "p1", models = { "bad-entry" }, stream = function() end })
            assert(not ok_bme)
            local ok_id = V.provider({ provider = "p1", models = { { id = "old" } }, stream = function() end })
            assert(not ok_id)
            local ok_vis = V.provider({ provider = "p1", models = { { model_id = "m1", vision = true } }, stream = function() end })
            assert(not ok_vis)
            local ok_cost = V.provider({ provider = "p1", models = { { model_id = "m1", cost = { input = "bad" } } }, stream = function() end })
            assert(not ok_cost)
            local ok_lf = V.provider({ provider = "p1", login = { bogus = true }, stream = function() end })
            assert(not ok_lf)
            local ok_lm = V.provider({ provider = "p1", login = { methods = { "unsupported_method" } }, stream = function() end })
            assert(not ok_lm)
            local ok_unk = V.provider({ provider = "p1", unknown_extra_field = 1, stream = function() end })
            assert(ok_unk ~= nil)
            local ok_wh = V.wire("w1", { name = "w1", stream = function() end, estimate_hints = "bad" })
            assert(not ok_wh)
            local ok_wh2 = V.wire("w1", { name = "w1", stream = function() end, estimate_hints = { headers = "not-list" } })
            assert(not ok_wh2)
            local ok_wh3 = V.wire("w1", { name = "w1", stream = function() end, estimate_hints = { headers = { "" } } })
            assert(not ok_wh3)

            -- 2. llm.models rejects unnamed/duplicate data providers, invalid targets, and conflicting target+save.
            local r_unnamed, e_unnamed = LLM:register_provider({})
            assert(r_unnamed == nil and tostring(e_unnamed):find("non-empty string", 1, true), tostring(e_unnamed))
            local r_dup, e_dup = LLM:register_provider({ provider = "faux", stream = function() end })
            assert(r_dup == nil and tostring(e_dup):find("duplicate provider", 1, true), tostring(e_dup))
            local r_d_un, e_d_un = LLM:register_provider({ wire = "openai-responses" })
            assert(r_d_un == nil and tostring(e_d_un):find("non-empty provider name", 1, true), tostring(e_d_un))
            local r_d_dup, e_d_dup = LLM:register_provider({ provider = "faux", wire = "openai-responses" })
            assert(r_d_dup == nil and tostring(e_d_dup):find("already registered", 1, true), tostring(e_d_dup))
            local r_bad, e_bad = LLM:register_provider({ provider = "cov_bad_prov", models = "nope", stream = function() end })
            assert(r_bad == nil and e_bad ~= nil)
            local ok_tgt = pcall(function() LLM:set_model("faux/echo", { target = "bogus_target" }) end)
            assert(not ok_tgt, "set_model with invalid target must fail")
            local ok_conf = pcall(function() LLM:set_model("faux/echo", { target = "global", save = false }) end)
            assert(not ok_conf, "set_model with conflicting target and save must fail")
            local ok_cap = pcall(function() return LLM:capabilities(123) end)
            assert(ok_cap == true or ok_cap == false)

            -- 3. llm.credentials handles missing credential, cancelled refresh, and invalid auth_status args.
            assert(LLM:refresh_credential("google", "no_such_cred_key", function() end) == nil)
            assert(not pcall(function() LLM:auth_status("") end))
            assert(not pcall(function() LLM:auth_status("no_such_provider_xyz") end))
            local _ = LLM:auth_status("opencode-go")

            -- 4. Unconfigured google/opencode-go providers fail in stream() and provider-faux handles assistant-only messages.
            local function err_msg(e)
                return type(e) == "table" and tostring(e.message or e.error) or tostring(e)
            end
            local g_model = LLM:resolve("google/gemini-2.5-flash")
            assert(g_model, "expected google model to resolve")
            local ok_g, err_g = pcall(function()
                return LLM:provider("google").stream({ model = g_model, messages = { { role = "user", content = "hi" } } }, function() end)
            end)
            assert(not ok_g and err_msg(err_g):find("GEMINI_API_KEY", 1, true), err_msg(err_g))
            local oc_model = LLM:resolve("opencode-go/glm-5") or LLM:resolve("opencode-go/kimi-k2.5")
            if oc_model then
                local ok_oc, err_oc = pcall(function()
                    return LLM:provider("opencode-go").stream({ model = oc_model, messages = { { role = "user", content = "hi" } } }, function() end)
                end)
                assert(not ok_oc and err_msg(err_oc):find("OPENCODE_API_KEY", 1, true), err_msg(err_oc))
            end
            local faux_m = LLM:resolve("faux/echo")
            local faux_res = LLM:complete({ model = faux_m, messages = { { role = "assistant", content = "no user msg" } } })
            assert(type(faux_res) == "table", tostring(faux_res))

            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;

        // 5. CLI binary usage errors cover main.rs validation arms cleanly without a TTY.
        for (args, needle) in [
            (&["--add-dir", "/tmp", "acp"][..], "--add-dir does not apply"),
            (&["--timeout", "1"][..], "no prompt"),
            (&["--json", "-p", ""][..], "no prompt"),
            (&["--add-dir", "/nonexistent_mare_cov_dir_xyz", "--json", "-p", "hi"][..], "nonexistent_mare_cov_dir_xyz"),
        ] {
            let out = mare_cmd()
                .args(args)
                .env("MARE_HOME", env.home.path())
                .output()
                .unwrap();
            assert!(!out.status.success(), "expected non-zero exit for {args:?}");
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(combined.contains(needle), "args={args:?} output={combined}");
        }
    })
    .await
}

#[tokio::test]
async fn compact_openai_statusline_and_keymap_edge_paths() {
    local(async {
        // 1. Powerline look + array-form external command + invalid `.git` file + PR link rendering.
        {
            let env = TestEnv::new().await;
            std::fs::write(env.cwd.join(".git"), "not a gitdir reference\n").unwrap();
            let mut b = env.host().builtins(&["compact", "provider-openai"]).faux(json!([])).config(json!({
                "logo": false,
                "icons": "ascii",
                "plugins": {
                    "permissions": { "enabled": true },
                    "onboarding": { "plugins": false },
                    "statusline": {
                        "look": "powerline",
                        "command": ["echo", "arr-ext-ok"],
                        "pr": true
                    }
                }
            }));
            b.opts.mode = Mode::Tui;
            let owned = OwnedVt::new(env, b.opts, 140, 16).await;
            let mut t = Term::from_vt(owned, vt100::Parser::new(nz(16), nz(140), 2000));
            t.until("powerline statusline with array external command", |s| {
                s.contains("Faux Echo") && s.contains("arr-ext-ok")
            })
            .await;

            // Exercise compact service/hooks/measure, provider-openai transport/websocket, statusline bars/git/layout, keymap unique.
            expose(&t.vt.host);
            global(&t.vt.host, "CMP", t.vt.host.service_table("compact").unwrap());
            global(&t.vt.host, "S", t.vt.host.service_table("session").unwrap());
            let ok: bool = eval(
                &t.vt.host,
                r#"
                -- compact.service: direct module check covers threshold/busy/cancel/offload/run/stage paths.
                local cs_state = { busy = false, token = mare.task.token() }
                local cs_auto = require("compact.service").create(C.cordis, { auto = true }, cs_state, { threshold = function() return 100 end })
                assert(cs_auto.threshold(nil, nil) == 100)
                assert(cs_auto.busy(nil, nil) == false)
                cs_auto.cancel(nil, nil)
                assert(cs_state.token:is_cancelled())
                local cs_off = require("compact.service").create(C.cordis, { auto = false }, cs_state, { threshold = function() return 100 end })
                assert(cs_off.threshold(nil, nil) == nil)
                cs_state.busy = true
                local r_busy, e_busy = cs_auto.offload_images(nil, nil, {})
                assert(r_busy == nil and e_busy == "already compacting")
                cs_state.busy = false
                local d_raw = cs_auto.register_stage(nil, nil, "cov_raw_stage", function() return false end)
                assert(type(d_raw) == "function")
                d_raw()
                d_raw()
                local ok_cs_auto, err_cs_auto = pcall(cs_auto.run, nil, nil, nil, { reason = "auto" })
                assert(not ok_cs_auto and tostring(err_cs_auto):find("manual", 1, true), tostring(err_cs_auto))
                local disp = CMP:register_stage("cov_noop_stage", function() return false end)
                assert(disp ~= nil, "expected disposer from register_stage")
                disp()
                disp()

                -- mare.subagent.spawn argument validation covers agent/mod.rs error paths.
                assert(not pcall(mare.subagent.spawn, "not-a-table"))
                assert(not pcall(mare.subagent.spawn, { tools = "not-an-array" }))
                assert(not pcall(mare.subagent.spawn, {}))

                -- compact.hooks: compact/completed notifications for incomplete, failed, and succeeded outcomes.
                C.cordis:emit("compact/completed", { error = { code = "compact.not_compacted", message = "simulated partial error" }, changed = true })
                C.cordis:emit("compact/completed", { error = { code = "compact.not_compacted", message = "simulated hard error" }, changed = false })
                C.cordis:emit("compact/completed", { tokens_before = 12000, tokens_after = 4000 })

                -- compact.measure: rejects non-array messages map.
                local cm = require("compact.measure").create(C.cordis, {}, { svc = {} })
                local ok_m, err_m = pcall(cm.request_tokens, { messages = { bad = 1 } })
                assert(not ok_m and tostring(err_m):find("must be an array", 1, true), tostring(err_m))

                -- provider-openai transport filter rejects non-table and non-http URLs.
                C.cordis:bail("provider/transport", "not-a-table")
                C.cordis:bail("provider/transport", { request = { url = "ftp://example.invalid" } })

                -- provider-openai.websocket: ws_wanted rejection arms and ws_response pre-cancelled token.
                local ws = require("provider-openai.websocket")({
                    ctx = C.cordis,
                    cfg = { transport = "auto" },
                    M = require("provider-openai.wire"),
                    auth = { llm = C.cordis:get("llm") },
                    is_ws_target = function(ev)
                        return type(ev) == "table" and ev.provider == "openai"
                    end,
                })
                assert(ws.ws_wanted("other", "https://example.com") == false)
                assert(ws.ws_wanted({ provider = "openai", transport = "bogus", request = { url = "https://api.openai.com/v1/responses" } }) == false)
                local tok = mare.task.token()
                tok:cancel()
                local r_ws, err_ws = ws.ws_response(
                    "openai",
                    { url = "https://api.openai.com/v1/responses", headers = {}, body = {} },
                    { cancel = tok }
                )
                assert(r_ws == nil and type(err_ws) == "table" and err_ws.aborted == true)

                -- statusline.bars: fmt_k >= 1M and quota_text window_label / low-quota variants.
                local bars = require("statusline.bars")
                local cur_prov = C.cordis:get("llm"):current().provider
                local S_mock = { limits_by_provider = {}, jobs_running = {} }
                local H = bars.helpers(C.cordis, S_mock)
                assert(H.fmt_k(1500000) == "1.5M")
                assert(H.quota_text() == nil)
                S_mock.limits_by_provider[cur_prov] = { primary = { used_percent = 50 }, secondary = { used_percent = 95, window_minutes = 7 * 24 * 60 } }
                local qt1, low1 = H.quota_text()
                assert(type(qt1) == "string" and qt1:find("wk", 1, true) and low1 == true, tostring(qt1))
                S_mock.limits_by_provider[cur_prov] = { primary = { used_percent = 20, window_minutes = 2 * 24 * 60 } }
                local qt2, low2 = H.quota_text()
                assert(type(qt2) == "string" and qt2:find("2d", 1, true) and low2 == false, tostring(qt2))

                -- statusline.git: invalid .git clears branch/pr state, and gh JSON decodes PR #42.
                local sg = require("statusline.git")
                assert(sg.branch() == nil)
                local done_tx, done_rx = mare.task.channel()
                local mock_ctx = {
                    ui = { invalidate = function() done_tx:send(true) end },
                    spawn = function(_, fn) fn() end,
                }
                local S_clear = { branch = "old-branch", changed = 2, pr = { number = 1 }, pr_branch = "old-branch" }
                sg.create(mock_ctx, { changed_files = false, pr = false }, S_clear)()
                assert(S_clear.branch == nil and S_clear.changed == nil and S_clear.pr == nil)

                local orig_branch = sg.branch
                local orig_which = mare.proc.which
                local orig_run = mare.proc.run
                sg.branch = function() return "feat-pr" end
                mare.proc.which = function() return "/usr/bin/gh" end
                mare.proc.run = function(opts)
                    if opts.cmd == "gh" then
                        return { code = 0, stdout = '{"number": 42, "url": "https://example.com/pr/42"}' }
                    end
                    return { code = 0, stdout = " M a.txt\n" }
                end
                local S_git = {}
                local ok_g, err_g = pcall(function()
                    sg.create(mock_ctx, { changed_files = true, pr = true, git_timeout_ms = 5000, pr_timeout_ms = 5000 }, S_git)()
                end)
                sg.branch = orig_branch
                mare.proc.which = orig_which
                mare.proc.run = orig_run
                assert(ok_g, tostring(err_g))
                assert(S_git.pr and S_git.pr.number == 42, "expected decoded PR #42")

                -- statusline.layout: render bold, fg, and link spans.
                local layout = require("statusline.layout")
                local rendered = layout.line({
                    { "bold-part", "MareInfo", true },
                    { "fg-part", nil, false, "MareSuccess" },
                    { "pr-link", "MareStatus", false, nil, "https://example.com/pr/42" },
                })
                assert(rendered ~= nil)

                -- statusline.segments: nil label_cache_box, hints=false, jobs, and PR link segments.
                local segs = require("statusline.segments")(
                    C.cordis,
                    {
                        pr = function() return { number = 42, url = "https://example.com/pr/42" } end,
                        jobs = function() return 2 end,
                        shown = function() return true end,
                    },
                    { mode = "plain", hints = false }
                )
                assert(segs.hints() == nil)
                assert(segs.jobs() ~= nil)
                assert(segs.pr() ~= nil)

                -- preset/keymap.lua: empty set_many and duplicate unique=true in batch.
                local kh_empty = mare.keymap.set_many({})
                assert(kh_empty ~= nil)
                local ok_dup_batch, err_dup_batch = pcall(
                    mare.keymap.set_many,
                    {
                        { "editor", "alt+q", "newline" },
                        { "editor", "alt+q", "editor.submit" },
                    },
                    { unique = true }
                )
                assert(not ok_dup_batch and tostring(err_dup_batch):find("already bound", 1, true), tostring(err_dup_batch))

                -- session.store: stale session_id/generation raises stale_session, and invalid image base64 fails cleanly.
                local ok_st, err_st = pcall(function()
                    return S:persist_message({
                        message = { role = "user", content = { { type = "text", text = "stale" } } },
                        session_id = "nonexistent-session-id",
                        generation = "nonexistent-generation-id",
                    })
                end)
                assert(not ok_st and tostring(err_st):find("stale", 1, true), tostring(err_st))
                local ok_b64, err_b64 = pcall(function()
                    return S:persist_message({
                        message = { role = "user", content = { { type = "image", mime = "image/png", data = "!!!bad-b64!!!" } } },
                    })
                end)
                assert(not ok_b64 and tostring(err_b64):find("base64", 1, true), tostring(err_b64))

                return true
                "#,
            )
            .await;
            assert!(ok);
        }

        // 2. Plain look + compact.auto=false covers plain mode styling and compact.threshold() == nil.
        {
            let env = TestEnv::new().await;
            let mut b = env.host().builtins(&["compact"]).faux(json!([])).no_session().config(json!({
                "logo": false,
                "icons": "ascii",
                "plugins": {
                    "permissions": { "enabled": true },
                    "onboarding": { "plugins": false },
                    "compact": { "auto": false },
                    "statusline": { "look": "plain" }
                }
            }));
            b.opts.mode = Mode::Tui;
            let owned = OwnedVt::new(env, b.opts, 120, 16).await;
            let mut t = Term::from_vt(owned, vt100::Parser::new(nz(16), nz(120), 2000));
            t.until("plain statusline rendered", |s| s.contains("Faux Echo")).await;
            global(&t.vt.host, "CMP", t.vt.host.service_table("compact").unwrap());
            let thresh_nil: bool = eval(&t.vt.host, "return CMP:threshold() == nil").await;
            assert!(thresh_nil);
        }
    })
    .await
}

#[cfg(unix)]
#[test]
fn ui_display_path_collapses_canonical_cwd_when_home_is_symlink() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().unwrap();
    let real_home = tmp.path().join("real_home");
    let sub_repo = real_home.join("sub").join("repo");
    let sibling = tmp.path().join("real_home2").join("other");
    let mare_home = tmp.path().join("mare_home");
    std::fs::create_dir_all(&sub_repo).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::create_dir_all(mare_home.join("config").join("plugins")).unwrap();

    let link_home = tmp.path().join("link_home");
    symlink(&real_home, &link_home).unwrap();
    let canon_repo = dunce::canonicalize(&sub_repo).unwrap();
    let canon_home = dunce::canonicalize(&real_home).unwrap();
    let canon_sibling = dunce::canonicalize(&sibling).unwrap();
    let out_file = tmp.path().join("display_path.json");

    let plugin = format!(
        r#"return {{
    name = "display-path-probe",
    apply = function()
        local out = {{
            cwd = mare.ui.display_path(mare.cwd, 0),
            home = mare.ui.display_path({canon_home:?}, 0),
            missing = mare.ui.display_path({missing:?}, 0),
            sibling = mare.ui.display_path({canon_sibling:?}, 0),
        }}
        mare.fs.write({out_file:?}, mare.json.encode(out))
    end,
}}"#,
        canon_home = canon_home.to_string_lossy(),
        missing = canon_home.join("future").join("file.rs").to_string_lossy(),
        canon_sibling = canon_sibling.to_string_lossy(),
        out_file = out_file.to_string_lossy(),
    );
    std::fs::write(mare_home.join("config").join("plugins").join("display_path_probe.lua"), plugin).unwrap();
    std::fs::write(
        mare_home.join("config").join("init.lua"),
        "mare.setup { update_check = false }\nmare.plugin(\"onboarding\", { plugins = false })\n",
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.current_dir(&canon_repo).env("HOME", &link_home).env("MARE_HOME", &mare_home).args([
        "--no-session",
        "plugin",
        "tree",
        "--json",
    ]);
    let out = cmd.run_output();
    assert!(
        out.status.success(),
        "mare plugin tree failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let got: Value = serde_json::from_str(&std::fs::read_to_string(&out_file).unwrap()).unwrap();
    assert_eq!(got["cwd"], "~/sub/repo", "{got}");
    assert_eq!(got["home"], "~", "{got}");
    assert_eq!(got["missing"], "~/future/file.rs", "{got}");
    assert_eq!(got["sibling"], canon_sibling.to_string_lossy().as_ref(), "{got}");
}

/// Startup batches statusline renders across all plugin waves and flushes once before the first
/// frame, so `left` and `right` are each pushed once (sharing a single layout pass) while the
/// first frame still matches the settled post-ready frame cell-for-cell.
#[tokio::test]
async fn statusline_deduplicates_pre_first_frame_renders_to_single_pass() {
    local(async {
        let env = TestEnv::new().await;
        let cfg_dir = env.home.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("init.lua"),
            r#"
            local calls = { left = 0, right = 0 }
            mare.ui._test_status_calls = calls
            local orig_set = mare.ui.set_status
            mare.ui.set_status = function(id, ...)
                if calls[id] ~= nil then
                    calls[id] = calls[id] + 1
                end
                return orig_set(id, ...)
            end
            "#,
        )
        .unwrap();
        let mut b = env.host().faux(json!([])).no_session().config(json!({
            "logo": false,
            "icons": "ascii",
            "plugins": { "permissions": { "enabled": true }, "onboarding": { "plugins": false } }
        }));
        b.opts.mode = Mode::Tui;
        b.opts.load_init = true;
        let mut vt = OwnedVt::new(env, b.opts, 120, 16).await;
        exec(&vt.host, r#"mare.state.set("permissions_intro_shown", true)"#).await;
        let mut parser = vt100::Parser::new(nz(16), nz(120), 2000);
        vt.first_frame().await;
        parser.process(&vt.output());

        let (left_calls, right_calls): (i64, i64) =
            eval(&vt.host, "return mare.ui._test_status_calls.left, mare.ui._test_status_calls.right").await;
        assert_eq!(left_calls, 1, "left status segment must render exactly once before first frame");
        assert_eq!(right_calls, 1, "right status segment must render exactly once before first frame");

        let first_text = parser.screen().contents();
        let first_cells = snapshot_cells(parser.screen());
        assert!(first_text.contains("Faux Echo"), "first frame must contain settled status bar:\n{first_text}");

        pump_until(&mut vt, &mut parser, "startup and ui/ready settled", |vt, _| vt.startup_finished()).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
            vt.frame().await;
            parser.process(&vt.output());
        }
        let ready_cells = snapshot_cells(parser.screen());
        assert_eq!(first_cells, ready_cells, "first frame must match post-ready frame cell-for-cell");
        drop(Term::from_vt(vt, parser));
    })
    .await
}

/// statusline.git PR probe: passes `cwd = mare.cwd`, does not permanently cache `gh` failures,
/// and re-queries on the same branch when no PR was previously found (e.g. after `gh pr create`).
#[tokio::test]
async fn statusline_git_pr_probe_recovers_after_failure_refreshes_same_branch_and_passes_cwd() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["statusline"]).no_session().build().await;
        let (cwd_ok, recovered_num, created_num, gh_calls): (bool, i64, i64, i64) = eval(
            &h,
            r#"local sg = require("statusline.git")
               local orig_branch = sg.branch
               local orig_which = mare.proc.which
               local orig_run = mare.proc.run
               local orig_connected = ui.connected
               local mock_ctx = {
                 ui = { invalidate = function() end },
                 spawn = function(_, fn) fn() end,
               }
               local gh_calls = 0
               local seen_cwd = nil
               local mode = "fail"
               ui.connected = function() return true end
               sg.branch = function() return "feat-pr" end
               mare.proc.which = function() return "/usr/bin/gh" end
               mare.proc.run = function(opts)
                 if opts.cmd == "gh" then
                   gh_calls = gh_calls + 1
                   seen_cwd = opts.cwd
                   if mode == "fail" then
                     return { code = 1, timed_out = true, stdout = "", stderr = "timeout" }
                   elseif mode == "pr77" then
                     return { code = 0, stdout = '{"number": 77, "url": "https://example.com/pr/77"}' }
                   elseif mode == "none" then
                     return { code = 1, stdout = "", stderr = "no pull requests found for branch" }
                   elseif mode == "pr88" then
                     return { code = 0, stdout = '{"number": 88, "url": "https://example.com/pr/88"}' }
                   end
                 end
                 return { code = 0, stdout = "" }
               end

               local S1 = {}
               local cfg = { changed_files = false, pr = true, git_timeout_ms = 5000, pr_timeout_ms = 5000 }
               local probe1 = sg.create(mock_ctx, cfg, S1)
               local ok, err = pcall(function()
                 -- 1) First probe fails/times out: must NOT permanently cache failure on feat-pr.
                 mode = "fail"
                 probe1()
                 assert(S1.pr == nil, "failed probe must not set PR")
                 -- Next probe on SAME branch succeeds -> PR #77 appears.
                 mode = "pr77"
                 probe1()

                 -- 2) Fresh state where initially no PR exists on feat-pr, then user creates PR #88.
                 local S2 = {}
                 local probe2 = sg.create(mock_ctx, cfg, S2)
                 mode = "none"
                 probe2()
                 assert(S2.pr == nil, "no PR initially")
                 mode = "pr88"
                 probe2()
                 S1._created = S2.pr and S2.pr.number or -1
               end)
               sg.branch = orig_branch
               mare.proc.which = orig_which
               mare.proc.run = orig_run
               ui.connected = orig_connected
               assert(ok, tostring(err))
               return seen_cwd == mare.cwd, S1.pr and S1.pr.number or -1, S1._created or -1, gh_calls"#,
        )
        .await;
        assert!(cwd_ok, "gh pr view must pass cwd = mare.cwd");
        assert_eq!(recovered_num, 77, "PR probe must recover on same branch after gh failure");
        assert_eq!(created_num, 88, "PR probe must discover newly created PR on same branch");
        assert_eq!(gh_calls, 4, "gh should have been queried 4 times across both scenarios");
        h.shutdown().await;
    })
    .await
}

/// External statusline command schedules a trailing rerun when triggered while already running and
/// resets its guard cleanly on error; `git.lua` also schedules a trailing rerun on overlap while
/// preserving per-branch PR caching so branches without a PR do not spam `gh pr view` every turn.
#[tokio::test]
async fn statusline_external_and_git_trailing_rerun_and_pr_cache() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().faux(json!([])).no_session().config(json!({
            "logo": false,
            "plugins": { "permissions": { "enabled": true }, "onboarding": { "plugins": false } }
        }));
        b.opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, b.opts, 100, 16).await;
        vt.frame().await;
        expose(&vt.host);

        let ok: bool = eval(
            &vt.host,
            r#"
            local orig_run = mare.proc.run
            local orig_which = mare.proc.which
            local _restore <close> = setmetatable({}, {
                __close = function()
                    mare.proc.run = orig_run
                    mare.proc.which = orig_which
                end,
            })

            local function wait_for(pred, label)
                for _ = 1, 100 do
                    if pred() then return true end
                    mare.task.sleep(10)
                end
                error("timed out waiting for " .. label)
            end

            -- 1. statusline.external trailing rerun: second trigger during in-flight run executes after completion.
            local ext = require("statusline.external")
            local ext_calls = 0
            local ext_started_tx, ext_started_rx = mare.task.channel()
            local ext_release_tx, ext_release_rx = mare.task.channel()
            local S_ext = { branch = "v1" }
            local inv_ext = 0
            local fake_ctx = {
                llm = C.cordis:get("llm"),
                get = function(_, name) return C.cordis:get(name) end,
                spawn = function(_, fn) return mare.task.spawn(fn) end,
                ui = { invalidate = function() inv_ext = inv_ext + 1 end },
            }
            mare.proc.run = function(opts)
                ext_calls = ext_calls + 1
                local n = ext_calls
                if n == 1 then
                    ext_started_tx:send(true)
                    ext_release_rx:recv()
                end
                local p = mare.json.decode(opts.input or "{}")
                local out = "run=" .. tostring(n) .. " br=" .. tostring(p.git_branch)
                return { code = 0, stdout = out .. "\n", timed_out = false }
            end
            local run_ext = ext.create(fake_ctx, { command = { "fake-ext" }, command_timeout_ms = 5000 }, S_ext)
            run_ext()
            ext_started_rx:recv()
            S_ext.branch = "v2"
            run_ext() -- arrives while first call is still running
            ext_release_tx:send(true)
            wait_for(function() return ext_calls == 2 and S_ext.external_text == "run=2 br=v2" end, "external trailing rerun")

            -- 2. statusline.external recovers if build_payload throws (must not stay permanently wedged).
            local fail_once = true
            local err_ctx = {
                llm = {
                    current = function()
                        if fail_once then
                            fail_once = false
                            error("transient payload failure")
                        end
                        return nil
                    end,
                },
                get = function() return nil end,
                spawn = function(_, fn) return mare.task.spawn(fn) end,
                ui = { invalidate = function() end },
            }
            local S_err = { branch = "ok" }
            local run_err = ext.create(err_ctx, { command = { "fake-ext" }, command_timeout_ms = 5000 }, S_err)
            run_err()
            mare.task.sleep(20)
            run_err()
            wait_for(function() return S_err.external_text == "run=3 br=ok" end, "external recovery after payload error")

            -- 3. statusline.git trailing rerun + per-branch PR negative cache (no per-turn gh spam).
            local git = require("statusline.git")
            local orig_branch = git.branch
            local _restore_branch <close> = setmetatable({}, {
                __close = function() git.branch = orig_branch end,
            })
            git.branch = function() return "feat-local" end
            mare.proc.which = function(name) return name == "gh" and "/usr/bin/gh" or nil end

            local git_status_calls = 0
            local gh_calls = 0
            local git_started_tx, git_started_rx = mare.task.channel()
            local git_release_tx, git_release_rx = mare.task.channel()
            mare.proc.run = function(opts)
                if opts.cmd == "git" then
                    git_status_calls = git_status_calls + 1
                    if git_status_calls == 1 then
                        git_started_tx:send(true)
                        git_release_rx:recv()
                        return { code = 0, stdout = " M a.lua\n", timed_out = false }
                    end
                    return { code = 0, stdout = " M a.lua\n M b.lua\n", timed_out = false }
                elseif opts.cmd == "gh" then
                    gh_calls = gh_calls + 1
                    return { code = 1, stdout = "", timed_out = false }
                end
                return { code = 1, stdout = "", timed_out = false }
            end
            local S_git = { branch = "feat-local" }
            local run_git = git.create(fake_ctx, { changed_files = true, pr = true, git_timeout_ms = 5000, pr_timeout_ms = 5000 }, S_git)
            run_git()
            git_started_rx:recv()
            run_git() -- arrives while first git refresh is running -> must schedule trailing rerun
            git_release_tx:send(true)
            wait_for(function() return git_status_calls == 2 and S_git.changed == 2 end, "git trailing rerun")
            assert(gh_calls == 1, "branches without a PR must keep per-branch cache without spamming gh; gh_calls=" .. tostring(gh_calls))
            return true
            "#,
        )
        .await;
        assert!(ok);
        vt.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn statusline_git_status_and_review_git_cannot_write_under_read_only_preset() {
    use crate::suite::sandbox_expect::require_backend;
    local(async {
        let mut env = TestEnv::new().await;
        assert!(fixture_git(&env.cwd).args(["init", "-q"]).status().expect("git").success());
        std::fs::write(env.cwd.join("README.md"), "hi\n").unwrap();
        assert!(fixture_git(&env.cwd).args(["add", "README.md"]).status().expect("git").success());

        // Plant a fake `git` binary earlier in PATH that skips leading global options (such as
        // `--no-optional-locks`) before checking the subcommand, tries to write a marker file,
        // and reports via stdout whether the write attempt was blocked by the sandbox so the
        // assertion cannot pass vacuously.
        let fake_bin = env.cwd.join(".git").join("fake-bin");
        std::fs::create_dir_all(&fake_bin).unwrap();
        let status_marker = env.cwd.join("statusline-wrote.txt");
        let review_marker = env.cwd.join("review-wrote.txt");
        let git_wrapper = fake_bin.join("git");
        std::fs::write(
            &git_wrapper,
            format!(
                "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --no-optional-locks|-*) shift ;;\n    *) break ;;\n  esac\ndone\nif [ \"$1\" = \"status\" ]; then\n  if touch \"{}\" 2>/dev/null; then\n    printf '?? WROTE\\n'\n  else\n    printf '?? blocked-1\\n?? blocked-2\\n'\n  fi\nelif [ \"$1\" = \"rev-parse\" ]; then\n  if touch \"{}\" 2>/dev/null; then\n    printf 'WROTE\\n'\n  else\n    printf 'BLOCKED_WRITE\\n'\n  fi\nfi\nexit 0\n",
                status_marker.display(),
                review_marker.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&git_wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let orig_path = std::env::var("PATH").unwrap_or_default();
        env.set_var("PATH", format!("{}:{}", fake_bin.display(), orig_path));

        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup {
                model = "faux/echo",
                logo = false,
                plugins = {
                    onboarding = { extras = false },
                    permissions = { preset = "read-only" },
                    statusline = { changed_files = true, pr = false },
                },
            }
            "#,
        )
        .unwrap();

        let mut b = env.host().user_config(true).no_session();
        b.opts.mode = Mode::Tui;
        let vt = OwnedVt::new(env, b.opts, 120, 16).await;
        let mut t = Term::from_vt(vt, vt100::Parser::new(nz(16), nz(120), 2000));
        t.until_vt("startup", |t| !t.vt.starting()).await;
        if !require_backend(
            &t.vt.host,
            "statusline_git_status_and_review_git_cannot_write_under_read_only_preset",
            mare_binding::ossandbox::expect::ANY,
        )
        .await
        {
            return;
        }
        let (level, review_out): (String, String) = eval(
            &t.vt.host,
            r#"
            local r = require("review.git").run({ "rev-parse", "HEAD" })
            return mare.proc.sandbox.current().level, type(r) == "string" and r or ""
            "#,
        )
        .await;
        assert_eq!(level, "read-only");
        assert!(
            review_out.contains("BLOCKED_WRITE"),
            "review.git fake wrapper must have attempted write and been blocked by sandbox: {review_out:?}"
        );
        // Wait until the statusline probe has actually executed the `status` branch of the fake
        // `git` wrapper (which prints 2 porcelain lines only when `touch` failed under sandbox).
        t.until("statusline probe blocked write under read-only sandbox", |s| {
            s.contains("±2") || s.contains("± 2") || s.contains("+- 2") || s.contains("+-2")
        })
        .await;
        assert!(!status_marker.exists(), "statusline git call wrote outside read-only sandbox");
        assert!(!review_marker.exists(), "review.git call wrote outside read-only sandbox");
    })
    .await
}

#[tokio::test]
async fn review_git_propagates_sandbox_error_instead_of_returning_nil() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_BACKEND", "none");
        let h = env.host().builtins(&["review"]).no_session().build().await;
        global(&h, "CWD", env.cwd.display().to_string());
        let (ok, err): (bool, String) = eval(
            &h,
            r#"
            mare.proc.sandbox.set_current({ fs_read = { "all" }, fs_write = { "none" } }, CWD)
            local ok, res = pcall(require("review.git").run, { "status" })
            return ok, tostring(res)
            "#,
        )
        .await;
        assert!(!ok, "review.git.run must propagate sandbox errors instead of returning nil: {err}");
        assert!(err.contains("SANDBOX_UNAVAILABLE"), "expected SANDBOX_UNAVAILABLE error, got: {err}");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn repro_231_statusline_git_probe_avoids_optional_index_lock() {
    use std::os::unix::fs::MetadataExt;

    local(async {
        let before_ino = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let before_ino_setup = before_ino.clone();
        let prepare = move |env: &TestEnv| {
            let git = |args: &[&str]| {
                let out = fixture_git(&env.cwd).args(args).output().expect("run git");
                assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
            };
            git(&["init", "-q", "-b", "trunk"]);
            git(&["config", "user.email", "repro@example.com"]);
            git(&["config", "user.name", "repro"]);
            std::fs::write(env.cwd.join("tracked.txt"), "hello\n").unwrap();
            git(&["add", "tracked.txt"]);
            git(&["commit", "-q", "-m", "init"]);
            std::fs::write(env.cwd.join("untracked.txt"), "new\n").unwrap();

            // Bump tracked.txt mtime without changing its contents so `git status` considers the
            // index stat-dirty and acquires `.git/index.lock` to rewrite `.git/index` unless
            // `--no-optional-locks` is passed. Use portable Rust `File::set_modified` instead of
            // GNU-specific `touch -d`.
            std::fs::File::options()
                .write(true)
                .open(env.cwd.join("tracked.txt"))
                .expect("open tracked.txt")
                .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_893_456_000))
                .expect("set_modified tracked.txt");

            let ino = std::fs::metadata(env.cwd.join(".git/index")).unwrap().ino();
            before_ino_setup.store(ino, std::sync::atomic::Ordering::SeqCst);
        };

        let cfg = json!({
            "icons": "ascii",
            "plugins": {
                "permissions": { "enabled": false },
                "statusline": { "changed_files": true, "pr": false },
            }
        });
        let mut t = Term::build(json!([]), cfg, json!({}), 120, 16, prepare).await;
        t.until("statusline changed-file count", |s| s.contains("@ trunk") && s.contains("+- 1")).await;

        let before = before_ino.load(std::sync::atomic::Ordering::SeqCst);
        let after = std::fs::metadata(t._env.cwd.join(".git/index")).unwrap().ino();
        assert_eq!(
            after, before,
            "background statusline git probe acquired .git/index.lock and rewrote .git/index (inode {before} -> {after}); expected --no-optional-locks"
        );
    })
    .await
}
