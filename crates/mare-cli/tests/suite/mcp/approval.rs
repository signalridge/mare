//! Approval of project servers, the disabled state, and approval of required servers.

use super::*;

fn project_trusted(b: HostBuilder) -> HostBuilder {
    let mut b = b;
    b.opts.project_trusted = true;
    b
}

/// Project servers (`.mcp.json`) start only once approved; the approval is keyed by name and a
/// hash of the configuration, so changing the command asks again. `--approve-mcps` approves.
#[tokio::test]
async fn mcp_project_servers_need_approval_keyed_by_config_hash() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "approve");
        let write = |extra: Value| {
            let entry = with(fake.entry(json!({})), with(json!({ "tier": "direct" }), extra));
            std::fs::write(env.cwd.join(".mcp.json"), json!({ "mcpServers": { "proj": entry } }).to_string()).unwrap();
        };
        write(json!({}));
        let state = |h: std::sync::Arc<mare_host::Host>| async move {
            eval::<String>(
                &h,
                r#"local req = { action = "list", lines = {}, code = 0 }
                   C.cordis:get("bus"):emit("mcp/cli", req) return table.concat(req.lines, "\n")"#,
            )
            .await
        };
        // not approved: not started
        let h = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
        expose(&h);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(state(h.clone()).await.contains("needs_approval"));
        assert_eq!(fake.count("start"), 0);
        h.shutdown().await;
        // --approve-mcps: approved and started; remembered
        let h =
            project_trusted(env.host().builtins(&["mcp"]).args(json!({ "no_session": true, "approve_mcps": true })))
                .build()
                .await;
        expose(&h);
        wait_until(15, "the approved server", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__proj__echo') ~= nil").await }
        })
        .await;
        h.shutdown().await;
        let approvals = std::fs::read_to_string(env.home.path().join("state/mcp-approvals.json")).unwrap();
        assert!(approvals.contains("proj|"), "{approvals}");
        let h = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
        expose(&h);
        wait_until(15, "the remembered approval", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__proj__echo') ~= nil").await }
        })
        .await;
        h.shutdown().await;
        // the command changed: asks again
        write(json!({ "env": { "EXTRA": "1" } }));
        let starts = fake.count("start");
        let h = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
        expose(&h);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(state(h.clone()).await.contains("needs_approval"));
        assert_eq!(fake.count("start"), starts);
        h.shutdown().await;
        assert_registered(env.home.path());
    })
    .await
}

/// `disable` / `enable` are kept per project in `<state>/mcp-disabled.json`; the config file is
/// never rewritten. The user-level `mcp.json` is a configuration source.
#[tokio::test]
async fn mcp_disabled_state_lives_in_the_state_dir_not_the_config() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "disable");
        let cfg_path = env.config_dir().join("mcp.json");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let text =
            json!({ "mcpServers": { "usr": with(fake.entry(json!({})), json!({ "tier": "direct" })) } }).to_string();
        std::fs::write(&cfg_path, &text).unwrap();
        let cli = |action: &'static str| {
            format!(
                r#"local req = {{ action = "{action}", server = "usr", lines = {{}}, code = 0 }}
                   C.cordis:get("bus"):emit("mcp/cli", req) return table.concat(req.lines, "\n")"#
            )
        };
        let h = env.host().builtins(&["mcp"]).no_session().build().await;
        expose(&h);
        wait_until(15, "the user server", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__usr__echo') ~= nil").await }
        })
        .await;
        let _: String = eval(&h, &cli("disable")).await;
        assert!(eval::<bool>(&h, "return T:get('mcp__usr__echo') == nil").await, "tools withdrawn");
        h.shutdown().await;
        assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), text, "config untouched");
        let state = std::fs::read_to_string(env.home.path().join("state/mcp-disabled.json")).unwrap();
        assert!(state.contains("usr"), "{state}");
        let starts = fake.count("start");
        let h = env.host().builtins(&["mcp"]).no_session().build().await;
        expose(&h);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(fake.count("start"), starts, "stays disabled");
        let _: String = eval(&h, &cli("enable")).await;
        wait_until(15, "the re-enabled server", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__usr__echo') ~= nil").await }
        })
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn required_mcp_request_waits_for_interactive_approval() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "startup-approval");
        let entry = with(fake.entry(json!({})), json!({ "tier": "direct", "required": true }));
        std::fs::write(env.cwd.join(".mcp.json"), json!({ "mcpServers": { "project": entry } }).to_string()).unwrap();
        let mut opts = env.host().builtins(&["mcp"]).faux(json!([{ "text": "approved" }])).no_session().opts;
        opts.mode = mare_shared::ui::Mode::Tui;
        opts.project_trusted = true;
        let ui = mare_shared::ui::UiShared::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *ui.tx.write().unwrap() = Some(tx);
        let h = mare_host::Host::with_ui(opts, ui).await.unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), async {
            loop {
                if let Some(mare_shared::ui::UiCmd::Select { title, reply, .. }) = rx.recv().await
                    && title.contains("Start the MCP server project")
                {
                    break reply;
                }
            }
        })
        .await
        .unwrap();
        let host = h.clone();
        let prompt = tokio::task::spawn_local(async move { host.prompt("hello", json!({})).await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!prompt.is_finished(), "required server was refused before its approval decision");
        reply.send(Ok(mare_shared::ui::SelectReply { index: 0, ..Default::default() })).unwrap();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), prompt).await.unwrap().unwrap().unwrap();
        assert_eq!(result["text"], "approved", "{result}");
        assert_eq!(fake.count("start"), 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn declined_required_mcp_approval_blocks_request_without_hanging() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "startup-decline");
        let entry = with(fake.entry(json!({})), json!({ "tier": "direct", "required": true }));
        std::fs::write(env.cwd.join(".mcp.json"), json!({ "mcpServers": { "project": entry } }).to_string()).unwrap();
        let mut opts = env.host().builtins(&["mcp"]).faux(json!([{ "text": "must not run" }])).no_session().opts;
        opts.mode = mare_shared::ui::Mode::Tui;
        opts.project_trusted = true;
        let ui = mare_shared::ui::UiShared::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *ui.tx.write().unwrap() = Some(tx);
        let h = mare_host::Host::with_ui(opts, ui).await.unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), async {
            loop {
                if let Some(mare_shared::ui::UiCmd::Select { title, reply, .. }) = rx.recv().await
                    && title.contains("Start the MCP server project")
                {
                    break reply;
                }
            }
        })
        .await
        .unwrap();
        let host = h.clone();
        let prompt = tokio::task::spawn_local(async move { host.prompt("hello", json!({})).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!prompt.is_finished());
        reply.send(Ok(mare_shared::ui::SelectReply { index: 1, ..Default::default() })).unwrap();
        // the ceiling only guards a hang; "without hanging" is the separate latency bound below
        let declined_at = std::time::Instant::now();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), prompt).await.unwrap().unwrap().unwrap();
        let took = declined_at.elapsed();
        assert!(took < std::time::Duration::from_secs(5), "the declined request took {took:?} to be refused");
        assert!(result["error"]["message"].as_str().unwrap_or("").contains("required MCP server project"), "{result}");
        assert_eq!(fake.count("start"), 0);
        h.shutdown().await;
    })
    .await
}

/// Changing `env_include` invalidates a previous project-server approval so the modified
/// configuration must be approved again.
#[tokio::test]
async fn mcp_changing_env_include_invalidates_approval() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "approve-env-include");
        let write = |extra: Value| {
            let entry = with(fake.entry(json!({})), with(json!({ "tier": "direct" }), extra));
            std::fs::write(env.cwd.join(".mcp.json"), json!({ "mcpServers": { "proj": entry } }).to_string()).unwrap();
        };
        write(json!({}));
        let state = |h: std::sync::Arc<mare_host::Host>| async move {
            eval::<String>(
                &h,
                r#"local req = { action = "list", lines = {}, code = 0 }
                   C.cordis:get("bus"):emit("mcp/cli", req) return table.concat(req.lines, "\n")"#,
            )
            .await
        };
        let h =
            project_trusted(env.host().builtins(&["mcp"]).args(json!({ "no_session": true, "approve_mcps": true })))
                .build()
                .await;
        expose(&h);
        wait_until(15, "the approved server", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__proj__echo') ~= nil").await }
        })
        .await;
        h.shutdown().await;

        write(json!({ "env_include": ["MCPT_API_KEY"] }));
        let starts = fake.count("start");
        let h = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
        expose(&h);
        wait_until(15, "needs_approval after env_include change", || {
            let h = h.clone();
            async move { state(h).await.contains("needs_approval") }
        })
        .await;
        assert_eq!(fake.count("start"), starts);
        h.shutdown().await;
    })
    .await
}

/// Changing tool-exposure fields (`tier`, `tools`, `enabled_tools`, `disabled_tools`) invalidates
/// a previous project-server approval so the modified configuration must be approved again.
#[tokio::test]
async fn mcp_changing_tool_exposure_fields_invalidates_approval() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "approve-exposure");
        let write = |extra: Value| {
            let entry = with(fake.entry(json!({})), with(json!({ "tier": "direct" }), extra));
            std::fs::write(env.cwd.join(".mcp.json"), json!({ "mcpServers": { "proj": entry } }).to_string()).unwrap();
        };
        let state = |h: std::sync::Arc<mare_host::Host>| async move {
            eval::<String>(
                &h,
                r#"local req = { action = "list", lines = {}, code = 0 }
                   C.cordis:get("bus"):emit("mcp/cli", req) return table.concat(req.lines, "\n")"#,
            )
            .await
        };

        for (label, mutation) in [
            ("tier", json!({ "tier": "deferred" })),
            ("tools", json!({ "tools": { "echo": "hidden" } })),
            ("enabled_tools", json!({ "enabled_tools": ["echo"] })),
            ("disabled_tools", json!({ "disabled_tools": ["echo"] })),
        ] {
            write(json!({}));
            let h = project_trusted(
                env.host().builtins(&["mcp"]).args(json!({ "no_session": true, "approve_mcps": true })),
            )
            .build()
            .await;
            expose(&h);
            wait_until(15, &format!("approved server before {label}"), || {
                let h = h.clone();
                async move { eval::<bool>(&h, "return T:get('mcp__proj__echo') ~= nil").await }
            })
            .await;
            h.shutdown().await;

            write(mutation);
            let starts = fake.count("start");
            let h = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
            expose(&h);
            wait_until(15, &format!("needs_approval after {label} change"), || {
                let h = h.clone();
                async move { state(h).await.contains("needs_approval") }
            })
            .await;
            assert_eq!(fake.count("start"), starts, "{label} change must not start unapproved server");
            h.shutdown().await;
        }
    })
    .await
}

/// Corrupt `mcp-disabled.json` or `mcp-approvals.json` must fail closed (never silently fall back
/// to `{}` and allow all servers) and must not be overwritten by subsequent state writes.
#[tokio::test]
async fn mcp_corrupt_disabled_and_approval_files_fail_closed() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "corrupt-state");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let usr_cfg = json!({ "mcpServers": { "usr": with(fake.entry(json!({})), json!({ "tier": "direct" })) } });
        std::fs::write(env.config_dir().join("mcp.json"), usr_cfg.to_string()).unwrap();

        let state_dir = env.home.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let disabled_path = state_dir.join("mcp-disabled.json");
        std::fs::write(&disabled_path, "{corrupt").unwrap();

        let h = env.host().builtins(&["mcp"]).no_session().build().await;
        expose(&h);
        let out: String = eval(
            &h,
            r#"local req = { action = "list", lines = {}, code = 0 }
               C.cordis:get("bus"):emit("mcp/cli", req)
               return req.code .. "\n" .. table.concat(req.lines, "\n")"#,
        )
        .await;
        assert!(out.starts_with("1\n"), "corrupt mcp-disabled.json must fail list: {out}");
        assert!(out.contains("invalid JSON") || out.contains("error"), "{out}");
        assert_eq!(fake.count("start"), 0, "corrupt mcp-disabled.json must not start servers");
        assert!(eval::<bool>(&h, "return T:get('mcp__usr__echo') == nil").await);

        let dout: String = eval(
            &h,
            r#"local req = { action = "disable", server = "usr", lines = {}, code = 0 }
               C.cordis:get("bus"):emit("mcp/cli", req)
               return req.code .. "\n" .. table.concat(req.lines, "\n")"#,
        )
        .await;
        assert!(dout.starts_with("1\n"), "disable on corrupt mcp-disabled.json must fail: {dout}");
        assert_eq!(std::fs::read_to_string(&disabled_path).unwrap(), "{corrupt");
        h.shutdown().await;

        std::fs::remove_file(&disabled_path).unwrap();
        let approvals_path = state_dir.join("mcp-approvals.json");
        std::fs::write(&approvals_path, "{corrupt").unwrap();
        let proj_cfg = json!({ "mcpServers": { "proj": with(fake.entry(json!({})), json!({ "tier": "direct" })) } });
        std::fs::write(env.cwd.join(".mcp.json"), proj_cfg.to_string()).unwrap();

        let h = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
        expose(&h);
        let aout: String = eval(
            &h,
            r#"local req = { action = "list", lines = {}, code = 0 }
               C.cordis:get("bus"):emit("mcp/cli", req)
               return req.code .. "\n" .. table.concat(req.lines, "\n")"#,
        )
        .await;
        assert!(aout.starts_with("1\n"), "list on corrupt mcp-approvals.json must fail: {aout}");
        assert!(aout.contains("invalid JSON") || aout.contains("error"), "{aout}");
        let _: () = eval(&h, r#"C.cordis:get("commands"):run("/mcp enable proj")"#).await;
        assert_eq!(std::fs::read_to_string(&approvals_path).unwrap(), "{corrupt");
        assert!(eval::<bool>(&h, "return T:get('mcp__proj__echo') == nil").await);
        h.shutdown().await;
    })
    .await
}

/// Concurrent `disable` and `approve` writes across hosts sharing `<state>` must not lose updates.
#[tokio::test]
async fn mcp_concurrent_disable_and_approve_writes_preserve_all_entries() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "concurrent-state");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let usr_cfg = json!({
            "mcpServers": {
                "s1": with(fake.entry(json!({})), json!({ "tier": "deferred" })),
                "s2": with(fake.entry(json!({})), json!({ "tier": "deferred" })),
            }
        });
        std::fs::write(env.config_dir().join("mcp.json"), usr_cfg.to_string()).unwrap();
        let proj_cfg = json!({
            "mcpServers": {
                "p1": with(fake.entry(json!({})), json!({ "tier": "deferred" })),
                "p2": with(fake.entry(json!({})), json!({ "tier": "deferred" })),
            }
        });
        std::fs::write(env.cwd.join(".mcp.json"), proj_cfg.to_string()).unwrap();

        let h1 = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
        let h2 = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
        expose(&h1);
        expose(&h2);

        let run_cli = |h: std::sync::Arc<mare_host::Host>, action: &'static str, server: &'static str| async move {
            eval::<i64>(
                &h,
                &format!(
                    r#"local req = {{ action = "{action}", server = "{server}", lines = {{}}, code = 0 }}
                       C.cordis:get("bus"):emit("mcp/cli", req)
                       return req.code"#
                ),
            )
            .await
        };
        let run_approve = |h: std::sync::Arc<mare_host::Host>, server: &'static str| async move {
            eval::<bool>(&h, &format!(r#"return C.cordis:get("commands"):run("/mcp enable {server}") ~= nil"#)).await
        };

        let (c1, c2, a1, a2) = tokio::join!(
            run_cli(h1.clone(), "disable", "s1"),
            run_cli(h2.clone(), "disable", "s2"),
            run_approve(h1.clone(), "p1"),
            run_approve(h2.clone(), "p2"),
        );
        assert_eq!((c1, c2, a1, a2), (0, 0, true, true));

        h1.shutdown().await;
        h2.shutdown().await;

        let disabled: Value =
            serde_json::from_str(&std::fs::read_to_string(env.home.path().join("state/mcp-disabled.json")).unwrap())
                .unwrap();
        let cwd_key = env.cwd.to_string_lossy().to_string();
        assert_eq!(disabled[&cwd_key]["s1"], true, "{disabled}");
        assert_eq!(disabled[&cwd_key]["s2"], true, "{disabled}");

        let approvals = std::fs::read_to_string(env.home.path().join("state/mcp-approvals.json")).unwrap();
        assert!(approvals.contains("p1|"), "{approvals}");
        assert!(approvals.contains("p2|"), "{approvals}");
    })
    .await
}

/// An explicit `false` value in `mcp-approvals.json` must not be treated as approved.
#[tokio::test]
async fn mcp_false_approval_record_does_not_grant_approval() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "false-approval");
        let entry = with(fake.entry(json!({})), json!({ "tier": "direct" }));
        std::fs::write(env.cwd.join(".mcp.json"), json!({ "mcpServers": { "proj": entry } }).to_string()).unwrap();

        let h =
            project_trusted(env.host().builtins(&["mcp"]).args(json!({ "no_session": true, "approve_mcps": true })))
                .build()
                .await;
        expose(&h);
        wait_until(15, "the approved server", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__proj__echo') ~= nil").await }
        })
        .await;
        h.shutdown().await;

        let approvals_path = env.home.path().join("state/mcp-approvals.json");
        let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&approvals_path).unwrap()).unwrap();
        let approved = doc["approved"].as_object_mut().unwrap();
        assert!(!approved.is_empty(), "{doc}");
        for v in approved.values_mut() {
            *v = json!(false);
        }
        std::fs::write(&approvals_path, doc.to_string()).unwrap();

        let starts = fake.count("start");
        let h = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
        expose(&h);
        let listing: String = eval(
            &h,
            r#"local req = { action = "list", lines = {}, code = 0 }
               C.cordis:get("bus"):emit("mcp/cli", req)
               return table.concat(req.lines, "\n")"#,
        )
        .await;
        assert!(listing.contains("needs_approval"), "{listing}");
        assert_eq!(fake.count("start"), starts, "false approval record must not start server");
        assert!(eval::<bool>(&h, "return T:get('mcp__proj__echo') == nil").await);
        h.shutdown().await;
    })
    .await
}

/// A project-layer partial override (e.g. modifying `tools` on an existing user server) must
/// trigger project approval and project trust checks rather than inheriting `user` provenance.
#[tokio::test]
async fn mcp_project_partial_override_on_user_server_requires_approval_and_trust() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "partial-override");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let user_entry = with(fake.entry(json!({})), json!({ "tier": "direct" }));
        std::fs::write(env.config_dir().join("mcp.json"), json!({ "mcpServers": { "usr": user_entry } }).to_string())
            .unwrap();
        // Project layer partially overrides only `tools` on the existing user server.
        std::fs::write(
            env.cwd.join(".mcp.json"),
            json!({ "mcpServers": { "usr": { "tools": { "echo": "direct" } } } }).to_string(),
        )
        .unwrap();

        let state = |h: std::sync::Arc<mare_host::Host>| async move {
            eval::<String>(
                &h,
                r#"local req = { action = "list", lines = {}, code = 0 }
                   C.cordis:get("bus"):emit("mcp/cli", req)
                   return table.concat(req.lines, "\n")"#,
            )
            .await
        };

        // Trusted project without approval: must require approval (not auto-start as user).
        let h = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
        expose(&h);
        let listing = state(h.clone()).await;
        assert!(listing.contains("needs_approval"), "expected needs_approval: {listing}");
        assert_eq!(fake.count("start"), 0);
        h.shutdown().await;

        // Untrusted project even with --approve-mcps: project stdio override must be blocked as untrusted.
        let mut untrusted = env.host().builtins(&["mcp"]).args(json!({ "no_session": true, "approve_mcps": true }));
        untrusted.opts.project_trusted = false;
        let h = untrusted.build().await;
        expose(&h);
        let listing = state(h.clone()).await;
        assert!(listing.contains("untrusted"), "expected untrusted: {listing}");
        assert_eq!(fake.count("start"), 0);
        h.shutdown().await;

        // Trusted project with --approve-mcps, from a fresh approval state: the untrusted run
        // above already persisted an approval, so drop it to prove this stage approves on its own.
        let approvals_path = env.home.path().join("state/mcp-approvals.json");
        let _ = std::fs::remove_file(&approvals_path);
        let h = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
        expose(&h);
        let listing = state(h.clone()).await;
        assert!(listing.contains("needs_approval"), "approval state was not reset: {listing}");
        h.shutdown().await;
        let h =
            project_trusted(env.host().builtins(&["mcp"]).args(json!({ "no_session": true, "approve_mcps": true })))
                .build()
                .await;
        expose(&h);
        wait_until(15, "the approved partially-overridden server", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__usr__echo') ~= nil").await }
        })
        .await;
        let approvals = std::fs::read_to_string(&approvals_path).unwrap();
        assert!(approvals.contains("usr|"), "trusted stage did not persist its approval: {approvals}");
        h.shutdown().await;

        // Changing `tools` in `.mcp.json` invalidates the fingerprint and requires approval again.
        std::fs::write(
            env.cwd.join(".mcp.json"),
            json!({ "mcpServers": { "usr": { "tools": { "echo": "deferred" } } } }).to_string(),
        )
        .unwrap();
        let starts = fake.count("start");
        let h = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
        expose(&h);
        let listing = state(h.clone()).await;
        assert!(listing.contains("needs_approval"), "changed tools must re-ask approval: {listing}");
        assert_eq!(fake.count("start"), starts);
        h.shutdown().await;
    })
    .await
}

/// A project overlay that changes only `enabled` or `required` on an approved server is a new
/// identity: an approval written by this version for the plain definition does not cover it
/// (CFG-SEC terra sec-0210 #2).
#[tokio::test]
async fn mcp_overlay_of_enabled_or_required_invalidates_existing_approval() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "overlay-enabled-required");
        let entry = with(fake.entry(json!({})), json!({ "tier": "direct" }));
        std::fs::write(env.cwd.join(".mcp.json"), json!({ "mcpServers": { "proj": entry } }).to_string()).unwrap();
        let state = |h: std::sync::Arc<mare_host::Host>| async move {
            eval::<String>(
                &h,
                r#"local req = { action = "list", lines = {}, code = 0 }
                   C.cordis:get("bus"):emit("mcp/cli", req) return table.concat(req.lines, "\n")"#,
            )
            .await
        };
        // The approval of the plain definition, written by the current version.
        let h =
            project_trusted(env.host().builtins(&["mcp"]).args(json!({ "no_session": true, "approve_mcps": true })))
                .build()
                .await;
        expose(&h);
        wait_until(15, "the approved server", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__proj__echo') ~= nil").await }
        })
        .await;
        h.shutdown().await;

        std::fs::create_dir_all(env.cwd.join(".mare")).unwrap();
        for overlay in
            [json!({ "enabled": true }), json!({ "required": true }), json!({ "enabled": true, "required": true })]
        {
            std::fs::write(env.cwd.join(".mare/mcp.json"), json!({ "mcpServers": { "proj": overlay } }).to_string())
                .unwrap();
            let starts = fake.count("start");
            let h = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
            expose(&h);
            let listing = state(h.clone()).await;
            assert!(listing.contains("needs_approval"), "overlay {overlay} reused the old approval: {listing}");
            assert_eq!(fake.count("start"), starts, "overlay {overlay} started without approval");
            h.shutdown().await;
        }
    })
    .await
}

/// Mutating `enabled`, `required`, or `description` individually inside an already-approved
/// project overlay file (keeping the exact same overlay source path) invalidates the approval.
#[tokio::test]
async fn mcp_same_overlay_field_mutations_invalidate_existing_approval() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "same-overlay-fields");
        let entry = with(fake.entry(json!({})), json!({ "tier": "direct" }));
        std::fs::write(env.cwd.join(".mcp.json"), json!({ "mcpServers": { "proj": entry } }).to_string()).unwrap();
        std::fs::create_dir_all(env.cwd.join(".mare")).unwrap();
        let overlay_path = env.cwd.join(".mare/mcp.json");
        std::fs::write(
            &overlay_path,
            json!({ "mcpServers": { "proj": { "required": false, "description": "v1" } } }).to_string(),
        )
        .unwrap();

        let state = |h: std::sync::Arc<mare_host::Host>| async move {
            eval::<String>(
                &h,
                r#"local req = { action = "list", lines = {}, code = 0 }
                   C.cordis:get("bus"):emit("mcp/cli", req) return table.concat(req.lines, "\n")"#,
            )
            .await
        };

        // Approve the server while the project overlay already exists.
        let h =
            project_trusted(env.host().builtins(&["mcp"]).args(json!({ "no_session": true, "approve_mcps": true })))
                .build()
                .await;
        expose(&h);
        wait_until(15, "the approved overlaid server", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__proj__echo') ~= nil").await }
        })
        .await;
        h.shutdown().await;

        // Mutate each field individually in the same overlay file (same overlay source path).
        let mutations = [
            ("enabled", json!({ "enabled": true, "required": false, "description": "v1" })),
            ("required", json!({ "required": true, "description": "v1" })),
            ("description", json!({ "required": false, "description": "v2" })),
        ];
        for (field, overlay) in mutations {
            std::fs::write(&overlay_path, json!({ "mcpServers": { "proj": overlay } }).to_string()).unwrap();
            let starts = fake.count("start");
            let h = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
            expose(&h);
            let listing = state(h.clone()).await;
            assert!(
                listing.contains("needs_approval"),
                "mutating {field} in same overlay reused old approval: {listing}"
            );
            assert_eq!(fake.count("start"), starts, "mutating {field} started server without re-approval");
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn cfg_root_loads_both_mcp_files_from_resource_root_and_keys_disabled_by_cwd() {
    local(async {
        let env = TestEnv::new().await;
        let repo = dunce::canonicalize(&env.cwd).unwrap();
        let sub = repo.join("sub/dir");
        std::fs::create_dir_all(repo.join(".git/objects")).unwrap();
        std::fs::create_dir_all(repo.join(".git/refs")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".mare")).unwrap();

        let fake_top = Fake::new(&env, "cfg-root-top");
        let fake_dot = Fake::new(&env, "cfg-root-dot");
        let entry_top = with(fake_top.entry(json!({})), json!({ "tier": "direct" }));
        let entry_dot = with(fake_dot.entry(json!({})), json!({ "tier": "direct" }));
        std::fs::write(repo.join(".mcp.json"), json!({ "mcpServers": { "proj_top": entry_top } }).to_string()).unwrap();
        std::fs::write(repo.join(".mare/mcp.json"), json!({ "mcpServers": { "proj_dot": entry_dot } }).to_string())
            .unwrap();
        mare_config::trust::set_dir(&repo, true).unwrap();

        let mut b = project_trusted(env.host().builtins(&["mcp"]).no_session());
        b.opts.cwd = sub.clone();
        let h = b.build().await;
        expose(&h);
        let listing: String = eval(
            &h,
            r#"local req = { action = "list", lines = {}, code = 0 }
               C.cordis:get("bus"):emit("mcp/cli", req)
               return table.concat(req.lines, "\n")"#,
        )
        .await;
        assert!(listing.contains("proj_top") && listing.contains("needs_approval"), "missing proj_top: {listing}");
        assert!(listing.contains("proj_dot") && listing.contains("needs_approval"), "missing proj_dot: {listing}");

        // `/mcp disable` persists in state keyed by start cwd (`sub`), NOT resource root (`repo`).
        let _: String = eval(
            &h,
            r#"local req = { action = "disable", server = "proj_top", lines = {}, code = 0 }
               C.cordis:get("bus"):emit("mcp/cli", req)
               return table.concat(req.lines, "\n")"#,
        )
        .await;
        h.shutdown().await;

        let disabled_raw = std::fs::read_to_string(env.home.path().join("state/mcp-disabled.json")).unwrap();
        let disabled: Value = serde_json::from_str(&disabled_raw).unwrap();
        let sub_key = sub.to_string_lossy().into_owned();
        let repo_key = repo.to_string_lossy().into_owned();
        assert_eq!(disabled[&sub_key]["proj_top"], json!(true), "disabled must be keyed by cwd {sub_key}: {disabled}");
        assert!(disabled.get(&repo_key).is_none(), "disabled must NOT be keyed by repo root {repo_key}: {disabled}");
    })
    .await
}

/// An untrusted / unapproved project MCP server must NOT expand `${VAR}` or read
/// `env_file` until trusted and approved, and approval prompts must redact secrets.
#[tokio::test]
async fn unapproved_project_server_defers_var_expansion_and_env_file() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_TEST_SECRET_TOKEN", "top-secret-expanded-value");
        std::fs::write(env.cwd.join(".env.mcp"), "FILE_SECRET=from-dotenv\n").unwrap();
        let cfg = json!({
            "mcpServers": {
                "proj": {
                    "type": "http",
                    "url": "https://example.com/mcp?token=${MARE_TEST_SECRET_TOKEN}",
                    "env_file": ".env.mcp",
                    "bearer_token_env_var": "MARE_TEST_SECRET_TOKEN"
                }
            }
        });
        std::fs::write(env.cwd.join(".mcp.json"), cfg.to_string()).unwrap();

        let h = env.host().builtins(&["mcp"]).args(json!({ "no_session": true, "trust": "untrusted" })).build().await;
        expose(&h);
        let status_json: String = eval(
            &h,
            r#"local req = { action = "status", lines = {}, code = 0 }
               C.cordis:get("bus"):emit("mcp/cli", req)
               return mare.json.encode(req)"#,
        )
        .await;
        assert!(
            !status_json.contains("top-secret-expanded-value") && !status_json.contains("from-dotenv"),
            "deferred config must not expand env vars before trust+approval: {status_json}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mcp_init_lua_rejects_camelcase_keys_with_hint_while_external_mcp_json_accepts_them() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "camel");
        let cfg = servers(json!({
            "bad_init": with(
                fake.entry(json!({})),
                json!({
                    "tier": "direct",
                    "enabledTools": ["echo"],
                    "oauth": { "clientId": "cid", "scope": "read" }
                }),
            ),
        }));
        // External .mcp.json continues accepting camelCase keys for interoperability.
        let ext_entry = with(
            fake.entry(json!({})),
            json!({
                "tier": "direct",
                "enabledTools": ["echo"],
                "oauth": { "clientId": "cid", "scope": "read" }
            }),
        );
        std::fs::write(env.cwd.join(".mcp.json"), json!({ "mcpServers": { "ext_camel": ext_entry } }).to_string())
            .unwrap();

        let h = project_trusted(env.host().builtins(&["mcp"]).config(cfg).no_session()).build().await;
        global(&h, "M", h.service_table("mcp").unwrap());
        let status_json: String = eval(&h, "return mare.json.encode(M:list_servers())").await;
        let status: Value = serde_json::from_str(&status_json).unwrap();
        let errors: Vec<String> =
            status["errors"].as_array().unwrap().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        let joined = errors.join("\n");
        assert!(
            joined.contains("enabledTools") && joined.contains("enabled_tools"),
            "init.lua camelCase key must be rejected with snake_case hint: {joined}"
        );
        assert!(
            joined.contains("clientId") && joined.contains("client_id"),
            "init.lua oauth camelCase key must be rejected with snake_case hint: {joined}"
        );
        assert!(
            joined.contains("scope") && joined.contains("scopes"),
            "init.lua oauth scope alias must be rejected with scopes hint: {joined}"
        );
        assert!(!joined.contains("ext_camel"), "external .mcp.json must still accept camelCase: {joined}");

        let by_name: std::collections::HashMap<String, Value> = status["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| (row["name"].as_str().unwrap().to_owned(), row.clone()))
            .collect();
        assert_eq!(by_name["bad_init"]["state"], "error", "{:?}", by_name["bad_init"]);
        assert_eq!(by_name["ext_camel"]["state"], "needs_approval", "{:?}", by_name["ext_camel"]);
        assert_eq!(fake.count("start"), 0, "rejected init.lua server must not start");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mcp_approval_hash_uses_mare_json_encode() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "hash-encode");
        let entry = json!({
            "command": fake.entry(json!({}))["command"],
            "args": [],
            "env": {},
            "tier": "deferred"
        });
        std::fs::write(env.cwd.join(".mcp.json"), json!({ "mcpServers": { "proj": entry } }).to_string()).unwrap();

        let h =
            project_trusted(env.host().builtins(&["mcp"]).args(json!({ "no_session": true, "approve_mcps": true })))
                .build()
                .await;
        let expected: String = eval(
            &h,
            &format!(
                r#"local identity = {{
                    command = {cmd},
                    args = mare.json.array(),
                    env = {{}},
                    tier = "deferred",
                }}
                return "proj|" .. mare.crypto.sha256(mare.json.encode(identity)):sub(1, 16)"#,
                cmd = serde_json::to_string(entry["command"].as_str().unwrap()).unwrap(),
            ),
        )
        .await;
        h.shutdown().await;

        let approvals: Value =
            serde_json::from_str(&std::fs::read_to_string(env.home.path().join("state/mcp-approvals.json")).unwrap())
                .unwrap();
        let keys: Vec<&String> = approvals["approved"].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec![&expected], "approval hash must match mare.json.encode(identity): {approvals}");
    })
    .await
}

/// `/mcp` offers `Enable` (not `Approve`) for a `needs_approval` server, and both the panel action
/// and `/mcp enable <server>` approve and start it.
#[tokio::test]
async fn mcp_panel_and_command_use_enable_action_for_needs_approval() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "enable-approval");
        let entry = with(fake.entry(json!({})), json!({ "tier": "direct" }));
        std::fs::write(env.cwd.join(".mcp.json"), json!({ "mcpServers": { "proj": entry } }).to_string()).unwrap();

        let h = project_trusted(env.host().builtins(&["mcp"]).no_session()).build().await;
        expose(&h);
        let (items, enabled_ok): (Vec<String>, bool) = eval(
            &h,
            r#"local captured
               local orig_connected = mare.ui.connected
               local orig_select = mare.ui.select
               local orig_notify = mare.ui.notify
               mare.ui.connected = function() return true end
               mare.ui.notify = function() end
               mare.ui.select = function(spec)
                 if spec.title == "MCP servers" then
                   return 1
                 elseif spec.title == "MCP proj" then
                   captured = spec.items
                   for idx, label in ipairs(spec.items) do
                     if label == "Enable" then return idx end
                   end
                   return nil
                 end
                 return 2
               end
               C.cordis:get("commands"):run("/mcp")
               mare.ui.connected = orig_connected
               mare.ui.select = orig_select
               mare.ui.notify = orig_notify
               local deadline = mare.task.clock() + 5000
               while not T:get("mcp__proj__echo") and mare.task.clock() < deadline do
                 mare.task.sleep(25)
               end
               return captured or {}, T:get("mcp__proj__echo") ~= nil"#,
        )
        .await;
        assert!(
            items.iter().any(|x| x == "Enable") && !items.iter().any(|x| x == "Approve"),
            "needs_approval action list must offer Enable instead of Approve, got: {items:?}"
        );
        assert!(enabled_ok, "selecting Enable on a needs_approval server must approve and start it");
        h.shutdown().await;
    })
    .await
}
