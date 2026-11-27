//! Presets and their sandbox levels, badges and what the plugin being off means.

use super::*;
use crate::suite::sandbox_expect::require_backend;

#[tokio::test]
async fn presets_map_to_sandbox_levels_and_badges() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        let (p0, l0, p1, l1, labels): (String, String, String, String, Vec<String>) = eval(
            &h,
            r#"
            local labels = {}
            C.cordis:on("permission/changed", function(ev) labels[#labels + 1] = ev.label end)
            local p0, l0 = P:preset(), mare.proc.sandbox.current().level
            P:set("read-only")
            local p1, l1 = P:preset(), mare.proc.sandbox.current().level
            P:set("full-access")
            assert(mare.proc.sandbox.current() == nil, "full access has no sandbox")
            return p0, l0, p1, l1, labels
            "#,
        )
        .await;
        assert_eq!((p0.as_str(), l0.as_str()), ("auto", "workspace-write"));
        assert_eq!((p1.as_str(), l1.as_str()), ("read-only", "read-only"));
        let read = if net_controlled(&h).await { "READ" } else { "PARTIAL" };
        assert_eq!(labels, [read, "FULL"]);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn explicit_deny_read_reaches_builtin_and_custom_presets() {
    local(async {
        let env = TestEnv::new().await;
        let first = env.cwd.join("first");
        let second = env.cwd.join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let h = host(
            &env,
            true,
            json!({ "deny_read": [first], "presets": { "custom-deny": { "deny_read": [second] } } }),
            json!({ "no_session": true }),
        )
        .await;
        let (default, custom): (Vec<String>, Vec<String>) = eval(
            &h,
            r#"local default = mare.proc.sandbox.current().deny_read
               assert(P:set("custom-deny"))
               return default, mare.proc.sandbox.current().deny_read"#,
        )
        .await;
        assert_eq!(default, [first.to_string_lossy().to_string()]);
        assert_eq!(custom, [second.to_string_lossy().to_string()]);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn untrusted_project_starts_read_only_unless_cli_permission_flag_overrides() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"mare.plugin("permissions", { enabled = true, preset = "full-access" })"#,
        )
        .unwrap();
        // 1. User init.lua `preset = "full-access"` is capped to `read-only` when untrusted.
        let h = env
            .host()
            .builtins(&["jobs", "tools-apply-patch"])
            .user_config(true)
            .args(json!({ "no_session": true }))
            .project_trusted(false)
            .build_exposed()
            .await;
        global(&h, "P", h.service_table("permissions").unwrap());
        assert_eq!(eval::<String>(&h, "return P:preset()").await, "read-only");
        h.shutdown().await;

        // 2. Non-flag CLI config (`--set` / `HostOptions.config` without `cli_flag_paths`) is
        // also capped to `read-only` when untrusted.
        let mut set_untrusted = env
            .host()
            .builtins(&["jobs", "tools-apply-patch"])
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "full-access" } } }))
            .args(json!({ "no_session": true }))
            .project_trusted(false);
        set_untrusted.opts.load_init = true;
        let h = set_untrusted.build_exposed().await;
        global(&h, "P", h.service_table("permissions").unwrap());
        let (preset, level): (String, String) =
            eval(&h, "local c = mare.proc.sandbox.current(); return P:preset(), c and c.level or 'none'").await;
        assert_eq!((preset.as_str(), level.as_str()), ("read-only", "read-only"));
        h.shutdown().await;

        // 3. Dedicated `--permission` / `--yolo` CLI flag overrides `untrusted_preset`.
        let h = host(
            &env,
            false,
            json!({ "preset": "full-access" }),
            json!({ "no_session": true, "cli_flag_paths": ["plugins.permissions.preset"] }),
        )
        .await;
        assert_eq!(eval::<String>(&h, "return P:preset()").await, "full-access");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn first_switch_to_full_access_is_confirmed_once() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        let asked: i64 = eval(
            &h,
            r#"
            local n = 0
            mare.ui.connected = function() return true end
            mare.ui.confirm = function() n = n + 1 return true end
            assert(P:set("full-access", { confirm = true }))
            P:set("auto")
            assert(P:set("full-access", { confirm = true }))
            return n
            "#,
        )
        .await;
        assert_eq!(asked, 1);
        // Even after confirming once in a trusted folder, switching into full-access
        // in an untrusted folder still requires confirmation.
        let untrusted_asked: i64 = eval(
            &h,
            r#"
            local n = 0
            mare.trusted = false
            P:set("read-only")
            mare.ui.confirm = function() n = n + 1 return true end
            assert(P:set("full-access", { confirm = true }))
            return n
            "#,
        )
        .await;
        assert_eq!(untrusted_asked, 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn unconfined_tools_need_approval_in_auto() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (d, reason): (String, String) = eval(
            &h,
            r#"local v = VOTE({ id = "m1", name = "mcp_search", arguments = {} }, { side_effect = "external", unconfined = true })
               return v.decision, v.reason"#,
        )
        .await;
        assert_eq!(d, "ask");
        assert!(reason.contains("not confined by the local sandbox"), "{reason}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn headless_denies_what_would_need_approval() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "x.txt", "content": "hi" } }] },
            { "text": "done" }
        ]);
        let mut b = env.host().faux(script).config(json!({ "plugins": { "permissions": { "enabled": true } } }));
        b = b.args(json!({ "no_session": true }));
        let h = b.build().await; // untrusted → read-only: the write asks, nobody can answer
        expose(&h);
        h.prompt("write it", json!({})).await.unwrap();
        let results = tool_results(&h).await;
        assert!(results.contains("Tool call denied"), "{results}");
        assert!(!env.cwd.join("x.txt").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn disabled_plugin_means_full_access() {
    local(async {
        let env = TestEnv::new().await;
        // the shared builder turns the plugin off: no service, no sandbox
        let h = env.host().no_session().build().await;
        assert!(h.service_table("permissions").is_none());
        assert!(eval::<bool>(&h, "return mare.proc.sandbox.current() == nil").await);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn minimal_preset_keeps_permissions_unless_explicitly_disabled() {
    local(async {
        let env = TestEnv::new().await;
        let mut builder = env.host();
        // Unlike the shared test builder, do not explicitly enable or disable permissions.
        builder.opts.config = json!({ "plugins": { "preset": "minimal" }, "model": "faux/echo" });
        let h = builder.no_session().build().await;
        assert!(h.service_table("permissions").is_some(), "minimal must retain default permissions");
        let level: String = eval(&h, "return mare.proc.sandbox.current().level").await;
        assert_eq!(level, "read-only", "an untrusted workspace remains read-only");
        h.shutdown().await;

        let h = env
            .host()
            .config(json!({ "plugins": { "preset": "minimal", "permissions": { "enabled": false } } }))
            .no_session()
            .build()
            .await;
        assert!(h.service_table("permissions").is_none(), "explicit disable still works");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn like_full_access_never_switches_the_sandbox_off() {
    local(async {
        let env = TestEnv::new().await;
        let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let target = dunce::canonicalize(outside.path()).unwrap().join("escaped.txt");
        // full-access approval behaviour (no prompts) on a read-only sandbox
        let presets = json!({ "quiet-ro": { "like": "full-access", "level": "read-only" } });
        let script = json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "command": create_file_cmd(&target) } }] },
            { "text": "done" }
        ]);
        let mut b = env.host().faux(script).no_session().config(
            json!({ "plugins": { "permissions": { "enabled": true, "preset": "quiet-ro", "presets": presets } } }),
        );
        b.opts.project_trusted = true;
        let h = b.build().await;
        if !require_backend(&h, "like_full_access_never_switches_the_sandbox_off", mare_binding::ossandbox::expect::ANY)
            .await
        {
            return h.shutdown().await;
        }
        expose(&h);
        exec(&h, VOTE).await;
        let (d, has_sandbox): (String, bool) =
            eval(&h, r#"local v = VOTE(BASH("touch x")) return v.decision, v.sandbox_grant ~= nil"#).await;
        assert_eq!((d.as_str(), has_sandbox), ("allow", false), "allowed without prompting, no per-call override");
        let res = h.prompt("go", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        assert!(!target.exists(), "the read-only sandbox must still apply: {res}");
        h.shutdown().await;
    })
    .await
}

/// Implicit network downgrade belongs only to the default Windows tier, not the enhanced tier.
#[tokio::test]
async fn enhanced_windows_requires_explicit_acceptance_of_missing_network_isolation() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        global(&h, "PRESETS_SOURCE", include_str!("../../../../mare-plugin/preset/plugins/lua/permissions/presets.lua"));
        let (default, enhanced): (bool, bool) = eval(
            &h,
            r#"local old_os, old_caps = mare.os, mare.proc.sandbox.capabilities
               mare.os = "windows"
               local build = assert(load(PRESETS_SOURCE, "=permissions-presets", "t"))()
               local function check(mechanism)
                   mare.proc.sandbox.capabilities = function() return { mechanism = mechanism, caps = { "fs_write" } } end
                   local presets = build({ cfg = {}, P = {}, ctx = {} })
                   return presets.win_default()
               end
               local default, enhanced = check("windows-acl"), check("windows-user")
               mare.os, mare.proc.sandbox.capabilities = old_os, old_caps
               return default, enhanced"#,
        ).await;
        assert!(default, "the default tier retains its documented PARTIAL default");
        assert!(!enhanced, "the enhanced tier must not silently accept missing network isolation");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn explicit_cli_permission_overrides_resumed_session_preset_on_startup_and_load() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "resumed-full-access.jsonl",
            &[
                json!({"type": "message", "entry_id": "m1", "parent_entry_id": null, "message": {"role": "user", "content": "hi"}}),
                json!({"type": "custom", "kind": "permission/preset", "data": {"preset": "full-access", "source": "user"}}),
            ],
        );
        // 1. Startup resume path: `--permission read-only --resume <path>`
        let h = host(
            &env,
            true,
            json!({ "preset": "read-only" }),
            json!({ "resume": path.to_string_lossy(), "cli_flag_paths": ["plugins.permissions.preset"] }),
        )
        .await;
        let (preset, level): (String, String) =
            eval(&h, "return P:preset(), mare.proc.sandbox.current().level").await;
        assert_eq!((preset.as_str(), level.as_str()), ("read-only", "read-only"));
        h.shutdown().await;

        // 2. `session/loaded` path: start with `--permission read-only`, then load a session with saved `full-access`
        let h = host(
            &env,
            true,
            json!({ "preset": "read-only" }),
            json!({ "cli_flag_paths": ["plugins.permissions.preset"] }),
        )
        .await;
        global(&h, "S", h.service_table("session").unwrap());
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, preset, level): (bool, String, String, String) = eval(
            &h,
            r#"
            local ok, err = pcall(S.load, S, { path = TARGET })
            return ok, tostring(err), P:preset(), mare.proc.sandbox.current().level
            "#,
        )
        .await;
        assert!(ok, "session load failed: {err}");
        assert_eq!((preset.as_str(), level.as_str()), ("read-only", "read-only"));
        h.shutdown().await;
    })
    .await
}

/// resuming or loading a session with a saved wider preset or custom `like = "full-access"`
/// preset in an untrusted project must cap both sandbox level and approval policy (`LIKE`) to the
/// current trust ceiling (`read-only` / `untrusted_preset`), while preserving narrower `read-only`
/// state and warning on downgrade.
#[tokio::test]
async fn resumed_session_preset_is_capped_by_untrusted_project_ceiling() {
    async fn assert_resumed_preset(
        env: &TestEnv,
        cfg: Value,
        path: &std::path::Path,
        want_preset: &str,
        want_level: &str,
        capped_from: Option<&str>,
    ) {
        let mut builder = env
            .host()
            .config(json!({"plugins":{"permissions":merge_into(json!({"enabled":true}), cfg)}}))
            .args(json!({ "resume": path.to_string_lossy() }));
        builder.opts.project_trusted = false;
        let ui = mare_shared::ui::UiShared::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *ui.tx.write().unwrap() = Some(tx);
        let h = mare_host::Host::with_ui(builder.opts, ui).await.unwrap();
        assert!(h.kernel.problems().is_empty(), "plugin problems: {:?}", h.kernel.problems());
        expose(&h);
        global(&h, "P", h.service_table("permissions").unwrap());
        let (preset, level): (String, String) =
            eval(&h, "local c = mare.proc.sandbox.current(); return P:preset(), c and c.level or 'none'").await;
        assert_eq!((preset.as_str(), level.as_str()), (want_preset, want_level));
        let mut notices = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            if let mare_shared::ui::UiCmd::Notify { text, level, .. } = cmd
                && text.contains("capped restored permission preset")
            {
                notices.push((text, level));
            }
        }
        if let Some(from) = capped_from {
            let expected = format!("capped restored permission preset from {from} to {want_preset}");
            assert!(notices.iter().any(|(text, level)| text.contains(&expected) && level == "warning"), "{notices:?}");
        } else {
            assert!(notices.is_empty(), "unchanged preset must not produce a cap notice: {notices:?}");
        }
        h.shutdown().await;
    }

    async fn assert_loaded_preset(
        env: &TestEnv,
        cfg: Value,
        path: &std::path::Path,
        want_preset: &str,
        want_level: &str,
    ) {
        let h = host(env, false, cfg, json!({})).await;
        global(&h, "S", h.service_table("session").unwrap());
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, preset, level): (bool, String, String, String) = eval(
            &h,
            r#"
            local ok, err = pcall(S.load, S, { path = TARGET })
            local c = mare.proc.sandbox.current()
            return ok, tostring(err), P:preset(), c and c.level or 'none'
            "#,
        )
        .await;
        assert!(ok, "session load failed: {err}");
        assert_eq!((preset.as_str(), level.as_str()), (want_preset, want_level));
        h.shutdown().await;
    }

    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "resumed-trusted-full-access.jsonl",
            &[
                json!({"type": "message", "entry_id": "m1", "parent_entry_id": null, "message": {"role": "user", "content": "hi"}}),
                json!({"type": "custom", "kind": "sandbox/mode", "provenance": "session-host", "data": {"level": "danger-full-access"}}),
                json!({"type": "custom", "kind": "permission/preset", "provenance": "session-host", "data": {"preset": "full-access", "source": "user"}}),
            ],
        );
        // 1. Startup resume & runtime `session:load` cap `full-access` -> `read-only` / `auto`.
        assert_resumed_preset(&env, json!({}), &path, "read-only", "read-only", Some("full-access")).await;
        assert_loaded_preset(&env, json!({ "untrusted_preset": "auto" }), &path, "auto", "workspace-write").await;

        // 2. Narrower saved preset (`read-only`) remains narrower under `untrusted_preset = "auto"`.
        let narrow_path = env.write_session(
            "resumed-trusted-readonly.jsonl",
            &[
                json!({"type": "message", "entry_id": "m1", "parent_entry_id": null, "message": {"role": "user", "content": "hi"}}),
                json!({"type": "custom", "kind": "sandbox/mode", "provenance": "session-host", "data": {"level": "read-only", "roots": [], "network": false}}),
                json!({"type": "custom", "kind": "permission/preset", "provenance": "session-host", "data": {"preset": "read-only", "source": "user"}}),
            ],
        );
        assert_resumed_preset(&env, json!({ "untrusted_preset": "auto" }), &narrow_path, "read-only", "read-only", None).await;

        // 3. Legacy `sandbox/mode`-only records (`danger-full-access` vs `read-only`).
        let mode_full = env.write_session(
            "resumed-trusted-mode-only.jsonl",
            &[
                json!({"type": "message", "entry_id": "m1", "parent_entry_id": null, "message": {"role": "user", "content": "hi"}}),
                json!({"type": "custom", "kind": "sandbox/mode", "provenance": "session-host", "data": {"level": "danger-full-access"}}),
            ],
        );
        assert_loaded_preset(&env, json!({}), &mode_full, "read-only", "read-only").await;

        let mode_ro = env.write_session(
            "resumed-trusted-mode-readonly.jsonl",
            &[
                json!({"type": "message", "entry_id": "m1", "parent_entry_id": null, "message": {"role": "user", "content": "hi"}}),
                json!({"type": "custom", "kind": "sandbox/mode", "provenance": "session-host", "data": {"level": "read-only", "roots": [], "network": false}}),
            ],
        );
        assert_loaded_preset(&env, json!({ "untrusted_preset": "auto" }), &mode_ro, "read-only", "read-only").await;

        // 4. Custom preset with `like = "full-access"` and `level = "read-only"` (`quiet-ro`) must
        // be capped in untrusted projects so `full-access` auto-allow never leaks into `read-only` or
        // `auto`, and real `tools:execute` still prompts for approval.
        let quiet_path = env.write_session(
            "resumed-trusted-quiet-ro.jsonl",
            &[
                json!({"type": "message", "entry_id": "m1", "parent_entry_id": null, "message": {"role": "user", "content": "hi"}}),
                json!({"type": "custom", "kind": "sandbox/mode", "provenance": "session-host", "data": {"level": "read-only", "roots": [], "network": false}}),
                json!({"type": "custom", "kind": "permission/preset", "provenance": "session-host", "data": {"preset": "quiet-ro", "source": "user"}}),
            ],
        );
        let custom_cfg = json!({ "presets": { "quiet-ro": { "like": "full-access", "level": "read-only" } } });
        assert_resumed_preset(&env, custom_cfg, &quiet_path, "read-only", "read-only", Some("quiet-ro")).await;

        let auto_custom_cfg = json!({
            "untrusted_preset": "auto",
            "presets": { "quiet-ro": { "like": "full-access", "level": "read-only" } },
        });
        let h = host(&env, false, auto_custom_cfg, json!({ "resume": quiet_path.to_string_lossy() })).await;
        let vote_json: String = eval(
            &h,
            r#"
            local asks = 0
            T:register("probe_unconfined", {
                description = "probe approval gating",
                unconfined = true,
                parameters = { type = "object", properties = {} },
                execute = function() return { content = "ok" } end,
            })
            local r = T:execute({ id = "p1", name = "probe_unconfined", arguments = {} }, {
                on_state = function() return true end,
                approve = function() asks = asks + 1 return { decision = "deny", reason = "blocked" } end,
            })
            return mare.json.encode({
                preset = P:preset(),
                asks = asks,
                is_error = r.is_error == true,
                refused = r.refused == true,
                content = r.content,
            })
            "#,
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&vote_json).unwrap();
        assert_eq!(v["preset"], json!("auto"), "capped quiet-ro must restore `auto`: {v}");
        assert_eq!(v["asks"], json!(1), "capped quiet-ro must prompt instead of full-access auto-allow: {v}");
        assert_eq!(v["is_error"], json!(true), "denied probe must error: {v}");
        h.shutdown().await;

        // 5. Sub-agent ceiling also enforces the `LIKE` dimension on custom presets.
        let mut b = env
            .host()
            .config(json!({
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "auto",
                        "presets": { "quiet-ro": { "like": "full-access", "level": "read-only" } }
                    }
                }
            }))
            .args(json!({ "no_session": true, "permission_ceiling": "auto" }));
        b.opts.mode = Mode::Child;
        b.opts.project_trusted = true;
        let child_h = b.build().await;
        expose(&child_h);
        global(&child_h, "P", child_h.service_table("permissions").unwrap());
        let (rejected, err): (bool, String) = eval(
            &child_h,
            r#"local ok, e = pcall(P.set, P, "quiet-ro"); return not ok, type(e) == "table" and tostring(e.message) or tostring(e)"#,
        )
        .await;
        assert!(rejected && err.contains("can't widen"), "child agent with auto ceiling must reject quiet-ro: {err}");
        child_h.shutdown().await;
    })
    .await
}

/// Codex `tui/src/app/tests/session_lifecycle_requests.rs:6057-6064`: a trust downgrade must
/// notify the user and remain enforced across subsequent session lifecycle operations.
#[tokio::test]
async fn quiet_ro_trust_downgrade_survives_fork_and_reload() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "untrusted_preset": "auto",
            "presets": {"quiet-ro": {"like": "full-access", "level": "read-only"}},
        });
        let h = host(&env, true, cfg, json!({})).await;
        exec(
            &h,
            r#"N = {}
               mare.ui.connected = function() return true end
               mare.ui.notify = function(message) N[#N + 1] = tostring(message) end"#,
        )
        .await;
        let turn = h.prompt("complete a turn before revoking trust", json!({})).await.unwrap();
        assert!(turn["error"].is_null(), "{turn}");
        exec(&h, r#"assert(P:set("quiet-ro")); assert(P:preset() == "quiet-ro"); N = {}"#).await;
        let workspaces = mare_host::WorkspaceRegistry::new();
        workspaces.attach("parent", &h);
        let id = h.workspace.id().clone();
        assert!(workspaces.set_trust(&id, false, false).await.unwrap());
        let (trusted, capped, trust_notice): (bool, String, String) =
            eval(&h, r#"return mare.trusted, C.cordis:get("permissions"):preset(), table.concat(N, "\n")"#).await;
        assert!(!trusted);
        assert_eq!(capped, "auto");
        assert!(trust_notice.contains("capped restored permission preset from quiet-ro to auto"), "{trust_notice}");

        let (forked, fork_preset, fork_record, fork_notice): (bool, String, String, String) = eval(
            &h,
            r#"local session = C.cordis:get("session")
               local before = session:info().session_id
               N = {}
               session:fork({session_id = before})
               local forked = session:info().session_id ~= before
               local preset = C.cordis:get("permissions"):preset()
               local records = session:list_records("permission/preset", {branch = "current"}).data
               local saved = records[#records] and records[#records].preset or ""
               local fork_notice = table.concat(N, "\n")
               return forked, preset, saved, fork_notice"#,
        )
        .await;
        assert!(forked);
        assert_eq!(fork_record, "quiet-ro", "fork must restore the saved wider approval preset");
        assert_eq!(fork_preset, "auto");
        assert!(fork_notice.contains("capped restored permission preset from quiet-ro to auto"), "{fork_notice}");

        let (reload_tx, reload_rx) = tokio::sync::oneshot::channel();
        let reload_tx = std::rc::Rc::new(std::cell::RefCell::new(Some(reload_tx)));
        let on_reload = h
            .lua()
            .create_function(move |_, _: mlua::Table| {
                if let Some(tx) = reload_tx.borrow_mut().take() {
                    let _ = tx.send(());
                }
                Ok(())
            })
            .unwrap();
        let (ctx, owner) = (h.kernel.host_ctx().unwrap(), h.kernel.host_owner().unwrap());
        h.kernel.host_on(&ctx, owner, "reload/completed", on_reload, mare_kernel::HostOn::default()).unwrap();
        exec(&h, r#"N = {}; C.cordis:get("commands"):run("/reload")"#).await;
        tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), reload_rx)
            .await
            .expect("/reload did not complete")
            .unwrap();
        let (reload_preset, reload_notice): (String, String) =
            eval(&h, r#"return C.cordis:get("permissions"):preset(), table.concat(N, "\n")"#).await;
        assert_eq!(reload_preset, "auto");
        assert!(reload_notice.contains("capped restored permission preset from quiet-ro to auto"), "{reload_notice}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn unrecognized_preset_or_untrusted_preset_errors_in_safe_state() {
    local(async {
        // 1. Misspelled `cfg.preset` ("read_only") must not fall back to `auto`.
        {
            let env = TestEnv::new().await;
            let mut b = env
                .host()
                .faux(json!([{ "text": "MUST NOT RUN" }]))
                .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "read_only" } } }));
            b.opts.project_trusted = true;
            let h = b.build().await;
            expose(&h);
            global(&h, "P", h.service_table("permissions").unwrap());
            assert_eq!(eval::<String>(&h, "return P:preset()").await, "off");
            let req = h.prompt("hello", json!({})).await.unwrap();
            assert_eq!(req["not_sent"], true, "{req}");
            assert!(req["error"]["message"].as_str().unwrap_or_default().contains("unknown preset"), "{req}");
            let verdict: String =
                eval(&h, r#"return C.cordis:get("policy"):decide("tool/before", { { id = "t1", name = "write", arguments = {} } }).decision"#)
                    .await;
            assert_eq!(verdict, "deny");
            h.shutdown().await;
        }

        // 2. Invalid `cfg.untrusted_preset` ("read_only") must not be skipped.
        {
            let env = TestEnv::new().await;
            let mut b = env
                .host()
                .faux(json!([{ "text": "MUST NOT RUN" }]))
                .config(json!({ "plugins": { "permissions": { "enabled": true, "untrusted_preset": "read_only" } } }));
            b.opts.project_trusted = false;
            let h = b.build().await;
            expose(&h);
            global(&h, "P", h.service_table("permissions").unwrap());
            assert_eq!(eval::<String>(&h, "return P:preset()").await, "off");
            let req = h.prompt("hello", json!({})).await.unwrap();
            assert_eq!(req["not_sent"], true, "{req}");
            assert!(req["error"]["message"].as_str().unwrap_or_default().contains("unknown untrusted_preset"), "{req}");
            let verdict: String =
                eval(&h, r#"return C.cordis:get("policy"):decide("tool/before", { { id = "t1", name = "write", arguments = {} } }).decision"#)
                    .await;
            assert_eq!(verdict, "deny");
            h.shutdown().await;
        }
    })
    .await
}

/// An invalid permission configuration keeps the safe state when a session saved with
/// `full-access` is resumed at startup or loaded at runtime: the saved preset must not reopen
/// tool execution (CFG-SEC terra sec-0210 #1).
#[tokio::test]
async fn invalid_preset_config_is_not_reopened_by_resumed_or_loaded_session() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "saved-full-access.jsonl",
            &[
                json!({"type": "message", "entry_id": "m1", "parent_entry_id": null, "message": {"role": "user", "content": "hi"}}),
                json!({"type": "custom", "kind": "permission/preset", "data": {"preset": "full-access", "source": "user"}}),
            ],
        );
        let build = |args: Value| {
            let mut b = env
                .host()
                .faux(json!([{ "text": "MUST NOT RUN" }]))
                .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "read_only" } } }))
                .args(args);
            b.opts.project_trusted = true;
            async move {
                let h = b.build().await;
                expose(&h);
                global(&h, "P", h.service_table("permissions").unwrap());
                h
            }
        };
        let assert_safe = |h: std::sync::Arc<mare_host::Host>, stage: &'static str| async move {
            assert_eq!(eval::<String>(&h, "return P:preset()").await, "off", "{stage}");
            let verdict: String =
                eval(&h, r#"return C.cordis:get("policy"):decide("tool/before", { { id = "t1", name = "write", arguments = {} } }).decision"#)
                    .await;
            assert_eq!(verdict, "deny", "{stage}");
            let req = h.prompt("hello", json!({})).await.unwrap();
            assert_eq!(req["not_sent"], true, "{stage}: {req}");
            assert!(req["error"]["message"].as_str().unwrap_or_default().contains("unknown preset"), "{stage}: {req}");
        };

        // 1. Startup resume of the saved session.
        let h = build(json!({ "resume": path.to_string_lossy() })).await;
        assert_safe(h.clone(), "startup resume").await;
        h.shutdown().await;

        // 2. Runtime `session/loaded` of the same session.
        let h = build(json!({})).await;
        global(&h, "S", h.service_table("session").unwrap());
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err): (bool, String) =
            eval(&h, "local ok, err = pcall(S.load, S, { path = TARGET }) return ok, tostring(err)").await;
        assert!(ok, "session load failed: {err}");
        assert_safe(h.clone(), "runtime load").await;
        h.shutdown().await;
    })
    .await
}

/// An untrusted project's `.mare/init.lua` cannot widen permissions (permissions corpus;
/// Codex core/src/exec_policy_tests.rs:569 ignores_rules_from_untrusted_project_layers, :2422;
/// core/tests/suite/worktree_trust.rs:20): its full-access preset, its allow rule for `curl` and
/// its approval policy `never` take no effect until the project is trusted (positive control:
/// the same file in a trusted project does take effect).
#[tokio::test]
async fn an_untrusted_project_init_lua_cannot_widen_permissions() {
    local(async {
        let mut seen = Vec::new();
        for trusted in [false, true] {
            let env = TestEnv::new().await;
            std::fs::create_dir_all(env.cwd.join(".mare")).unwrap();
            std::fs::write(
                env.cwd.join(".mare/init.lua"),
                r#"mare.plugin("permissions", { preset = "full-access",
                       rules = { { argv = { "curl" }, decision = "allow", scope = "all" } } })
                   mare.plugin("approvals", { policy = "never" })"#,
            )
            .unwrap();
            let h = env
                .host()
                .config(json!({ "plugins": { "permissions": { "enabled": true } } }))
                .user_config(trusted)
                .no_session()
                .build()
                .await;
            expose(&h);
            global(&h, "P", h.service_table("permissions").unwrap());
            exec(&h, VOTE).await;
            let got: (String, String, String, String) = eval(
                &h,
                r#"local v = VOTE(BASH("curl http://example.invalid"))
                   return P:preset(), C.cordis:get("approvals"):policy(), tostring(v.decision), tostring(v.basis)"#,
            )
            .await;
            h.shutdown().await;
            seen.push((trusted, got));
        }
        let (untrusted, trusted) = (&seen[0].1, &seen[1].1);
        assert_eq!(trusted.0, "full-access", "control: a trusted project's init.lua applies: {seen:?}");
        assert_eq!(trusted.1, "never", "control: {seen:?}");
        assert_ne!(untrusted.0, "full-access", "an untrusted project set the preset: {seen:?}");
        assert_ne!(untrusted.1, "never", "an untrusted project set the approval policy: {seen:?}");
        assert!(
            !(untrusted.2 == "allow" && (untrusted.3 == "rule" || untrusted.3 == "full-access")),
            "an untrusted project's rule or preset allowed curl: {seen:?}"
        );
    })
    .await
}

/// A targeted reload of `permissions` keeps the preset the session recorded. Its restore runs in
/// `apply` before the session follower is filled; it reads the active session, and applies the
/// sandbox through that same instance, instead of falling back to the configured preset (reference implementation
/// sandbox-policy policy.spec.ts:115: a session's own read-only wins over the default). The
/// rules file's stat / read answer at once during the reload: today they are the only thing that
/// yields before the restore (permissions/rules_file.lua refresh), and the restore must not
/// depend on that.
#[tokio::test]
async fn a_targeted_reload_of_permissions_keeps_the_sessions_preset() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"mare.plugin("permissions", { enabled = true, preset = "auto" })"#,
        )
        .unwrap();
        let h = env
            .host()
            .builtins(&["jobs", "tools-apply-patch"])
            .user_config(true)
            .project_trusted(true)
            .build_exposed()
            .await;
        global(&h, "P", h.service_table("permissions").unwrap());
        let (before, preset, level): (String, String, String) = eval(
            &h,
            r#"
            assert(P:set("read-only"))
            local before = P:preset()
            local kernel = C.cordis:get("kernel")
            local stat, read = mare.fs.stat, mare.fs.read
            mare.fs.stat, mare.fs.read = function() return nil end, function() return nil end
            local ok, err = pcall(function()
                kernel:reload("permissions")
                kernel:reload_barrier()
            end)
            mare.fs.stat, mare.fs.read = stat, read
            assert(ok, err)
            return before, C.cordis:get("permissions"):preset(), mare.proc.sandbox.current().level
            "#,
        )
        .await;
        assert_eq!((before.as_str(), preset.as_str(), level.as_str()), ("read-only", "read-only", "read-only"));
        h.shutdown().await;
    })
    .await
}

/// Three consecutive similar approvals offer to switch to a wider preset via
/// `mare.ui.select`, and reset the streak afterwards.
#[tokio::test]
async fn three_consecutive_similar_approvals_suggest_wider_preset() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "auto", "suggest_after": 3 }), json!({})).await;
        exec(
            &h,
            r#"
            _TEST_PROMPTS = {}
            _TEST_ORIG_CONN, _TEST_ORIG_SELECT = mare.ui.connected, mare.ui.select
            mare.ui.connected = function() return true end
            mare.ui.select = function(opts)
                local labels = {}
                for _, item in ipairs((opts and opts.items) or {}) do
                    labels[#labels + 1] = tostring(item.label or "")
                end
                _TEST_PROMPTS[#_TEST_PROMPTS + 1] = tostring(opts and opts.title or "") .. " | " .. table.concat(labels, ", ")
                return 2 -- Keep current preset
            end
            for i = 1, 4 do
                C.cordis:emit("pipeline/approval-decided", {
                    decision = "allow",
                    call = { id = "c" .. i, name = "bash", arguments = { command = "echo " .. i } },
                })
            end
            "#,
        )
        .await;
        wait_until(WAIT_SECS, "wider preset suggestion select prompt", || {
            let h = h.clone();
            async move { eval::<usize>(&h, "return #_TEST_PROMPTS").await >= 1 }
        })
        .await;
        tokio::task::yield_now().await;
        let suggestions: Vec<String> = eval(
            &h,
            r#"
            mare.ui.connected, mare.ui.select = _TEST_ORIG_CONN, _TEST_ORIG_SELECT
            return _TEST_PROMPTS
            "#,
        )
        .await;
        assert_eq!(
            suggestions.len(),
            1,
            "expected exactly 1 wider-preset suggestion after 3 approvals (streak resets on 4th): {suggestions:?}"
        );
        assert!(
            suggestions[0].contains("3 similar requests") && suggestions[0].contains("Switch to full-access"),
            "{suggestions:?}"
        );
        h.shutdown().await;
    })
    .await
}

/// First launch with connected UI emits the permissions intro notice once and records
/// `permissions_intro_shown` in `state.json`.
#[tokio::test]
async fn first_run_emits_permissions_intro_notice_once() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "auto" } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"
            _G.__PRESET_INTRO_NOTICES = {}
            _G.__PRESET_ORIG_CONN, _G.__PRESET_ORIG_NOTIFY = mare.ui.connected, mare.ui.notify
            mare.ui.connected = function() return true end
            mare.ui.notify = function(msg)
              _G.__PRESET_INTRO_NOTICES[#_G.__PRESET_INTRO_NOTICES + 1] = tostring(msg)
            end
            "#,
        )
        .await;
        h.kernel.emit_json("app/ready", json!({})).await;
        let (first_count, first_text): (i64, String) =
            eval(&h, "return #_G.__PRESET_INTRO_NOTICES, _G.__PRESET_INTRO_NOTICES[1] or ''").await;
        h.kernel.emit_json("app/ready", json!({})).await;
        let (second_count, persisted): (i64, bool) = eval(
            &h,
            r#"
            local n2 = #_G.__PRESET_INTRO_NOTICES
            mare.ui.connected, mare.ui.notify = _G.__PRESET_ORIG_CONN, _G.__PRESET_ORIG_NOTIFY
            local st = mare.state.read() or {}
            return n2, st.permissions_intro_shown == true
            "#,
        )
        .await;
        assert_eq!(first_count, 1, "expected 1 intro notice on first app/ready: {first_text}");
        assert_eq!(second_count, 1, "second app/ready must not repeat intro notice");
        assert!(first_text.contains("Permissions:") && first_text.contains("/permission"), "{first_text}");
        assert!(persisted, "permissions_intro_shown must be persisted to state.json");
        h.shutdown().await;
    })
    .await
}

/// Runtime option writes to `plugins.permissions.preset` or `plugins.approvals.policy` must not
/// overwrite the startup CLI override snapshot, so subsequent `/new` and `/resume` transitions
/// continue to apply the original CLI values rather than promoting a runtime write to `source="cli"`.
#[tokio::test]
async fn runtime_config_writes_do_not_mutate_cli_permission_or_approval_overrides() {
    local(async {
        let env = TestEnv::new().await;
        let saved_path = env.write_session(
            "saved-full.jsonl",
            &[
                json!({ "type": "message", "entry_id": "m1", "parent_entry_id": null, "message": { "role": "user", "content": "hi" } }),
                json!({ "type": "custom", "kind": "permission/preset", "provenance": "session-host", "data": { "preset": "full-access", "source": "user" } }),
                json!({ "type": "custom", "kind": "approval/policy", "provenance": "session-host", "data": { "policy": "ask" } }),
            ],
        );
        let mut b = env
            .host()
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "read-only" },
                    "approvals": { "policy": "never" }
                }
            }))
            .args(json!({
                "cli_flag_paths": ["plugins.permissions.preset", "plugins.approvals.policy"]
            }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "P", h.service_table("permissions").unwrap());
        global(&h, "Q", h.service_table("approvals").unwrap());
        global(&h, "SAVED_PATH", saved_path.to_string_lossy().to_string());

        let (init_p, init_q): (String, String) = eval(&h, "return P:preset(), Q:policy()").await;
        assert_eq!((init_p.as_str(), init_q.as_str()), ("read-only", "never"));

        // Mutate both options at runtime via the `options` service.
        exec(
            &h,
            r#"
            C.cordis:get("options"):set({
                ["plugins.permissions.preset"] = "full-access",
                ["plugins.approvals.policy"] = "ask",
            })
            "#,
        )
        .await;

        // A new session must still inherit the startup CLI values (`read-only` / `never`),
        // not the mutated runtime values (`full-access` / `ask`).
        exec(&h, "S:create()").await;
        let (new_p, new_q): (String, String) =
            eval(&h, r#"return C.cordis:get("permissions"):preset(), C.cordis:get("approvals"):policy()"#).await;
        assert_eq!(new_p, "read-only", "runtime write must not leak as CLI preset on /new");
        assert_eq!(new_q, "never", "runtime write must not leak as CLI approval policy on /new");

        // Resuming a saved `full-access` / `ask` session must also keep the original CLI values.
        exec(&h, "S:load({ path = SAVED_PATH })").await;
        let (res_p, res_q): (String, String) =
            eval(&h, r#"return C.cordis:get("permissions"):preset(), C.cordis:get("approvals"):policy()"#).await;
        assert_eq!(res_p, "read-only", "runtime write must not leak as CLI preset on /resume");
        assert_eq!(res_q, "never", "runtime write must not leak as CLI approval policy on /resume");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn enforce_sandbox_ceiling_fails_closed_when_snapshot_probe_errors() {
    local(async {
        let env = TestEnv::new().await;
        for failing_fn in ["snapshot_current", "snapshot_policy"] {
            let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
            global(&h, "FAIL_FN", failing_fn.to_owned());
            let (preset_after, tool_denied, tool_text, req_decision): (String, bool, String, String) = eval(
                &h,
                r#"
                T:register("probe_ceiling_fail_closed", {
                    description = "probe tool",
                    parameters = { type = "object", properties = {} },
                    execute = function() return { content = "ran" } end,
                })
                local orig = mare.proc.sandbox[FAIL_FN]
                mare.proc.sandbox[FAIL_FN] = function(...)
                    error("injected " .. FAIL_FN .. " failure")
                end
                C.cordis:get("bus"):emit("session/new", {
                    restore_permission_ceiling = "read-only",
                    restore_sandbox_ceiling = { level = "read-only", network = false },
                })
                mare.proc.sandbox[FAIL_FN] = orig
                local preset = P:preset()
                local tool_res = T:execute({ id = "p1", name = "probe_ceiling_fail_closed", arguments = {} })
                local tool_text = type(tool_res.content) == "string" and tool_res.content
                    or mare.json.encode(tool_res.content)
                local tool_denied = tool_res.is_error == true
                    and tool_text:find("injected " .. FAIL_FN .. " failure", 1, true) ~= nil
                local req = C.cordis:get("policy"):decide("request/ready", { {} })
                return preset, tool_denied, tool_text, tostring(req and req.decision or "nil")
                "#,
            )
            .await;
            assert_eq!(preset_after, "off", "{failing_fn} failure must put permissions in fail-closed off state");
            assert!(tool_denied, "{failing_fn} failure must deny subsequent tool execution: {tool_text}");
            assert_eq!(req_decision, "deny", "{failing_fn} failure must deny subsequent request/ready");
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn sandbox_mode_only_readonly_resume_preserves_readable_and_deny_read_under_ceiling() {
    local(async {
        let env = TestEnv::new().await;
        let allowed_dir = env.cwd.join("allowed-read");
        let secret_dir = env.cwd.join("session-secret");
        let host_secret_dir = env.cwd.join("host-secret");
        std::fs::create_dir_all(&allowed_dir).unwrap();
        std::fs::create_dir_all(&secret_dir).unwrap();
        std::fs::create_dir_all(&host_secret_dir).unwrap();
        let allowed_canon = mare_host::session_orchestrator::canonical_dir(&allowed_dir).to_string_lossy().into_owned();
        let secret_canon = mare_host::session_orchestrator::canonical_dir(&secret_dir).to_string_lossy().into_owned();
        let host_secret_canon = mare_host::session_orchestrator::canonical_dir(&host_secret_dir).to_string_lossy().into_owned();

        // Session has ONLY a `sandbox/mode` record (no `permission/preset`) with custom read restrictions.
        let path = env.write_session(
            "mode-only-restricted-ro.jsonl",
            &[
                json!({"type": "message", "entry_id": "m1", "parent_entry_id": null, "message": {"role": "user", "content": "hi"}}),
                json!({
                    "type": "custom",
                    "kind": "sandbox/mode",
                    "provenance": "session-host",
                    "data": {
                        "level": "read-only",
                        "restrict_read": true,
                        "readable": [allowed_canon],
                        "deny_read": [secret_canon],
                        "network": false,
                        "accept_partial": ["fs_protect", "fs_read", "fs_deny_read"]
                    }
                }),
            ],
        );

        // (a) Trusted host loading under a looser non-admin `auto` ceiling + host-level `deny_read`.
        let cfg = json!({
            "preset": "auto",
            "deny_read": [host_secret_canon],
            "accept_partial": ["fs_protect", "fs_read", "fs_deny_read"]
        });
        let h = host(&env, true, cfg.clone(), json!({})).await;
        global(&h, "LOAD_PATH", path.to_string_lossy().to_string());
        let (preset, level, restrict_read, readable_json, deny_json): (String, String, bool, String, String) = eval(
            &h,
            r#"
            local ok, err = pcall(function()
                S:switch({
                    kind = "load",
                    path = LOAD_PATH,
                    restore_permission_ceiling = "auto",
                    restore_sandbox_ceiling = {
                        level = "workspace-write",
                        network = false,
                        accept_partial = { "fs_protect", "fs_read", "fs_deny_read" },
                    },
                })
            end)
            assert(ok, "session load failed: " .. tostring(err))
            local cur = mare.proc.sandbox.current()
            return P:preset(),
                cur and cur.level or "none",
                cur and cur.restrict_read == true or false,
                mare.json.encode(cur and cur.readable or {}),
                mare.json.encode(cur and cur.deny_read or {})
            "#,
        )
        .await;
        assert_eq!(preset, "read-only");
        assert_eq!(level, "read-only");
        assert!(restrict_read, "session restrict_read must survive read-only mode restore under ceiling");
        let readable: Vec<String> = serde_json::from_str(&readable_json).unwrap();
        let deny_read: Vec<String> = serde_json::from_str(&deny_json).unwrap();
        assert!(
            readable.iter().any(|p| mare_host::session_orchestrator::canonical_dir(std::path::Path::new(p)) == mare_host::session_orchestrator::canonical_dir(&allowed_dir)),
            "session readable dir must survive restore under broader ceiling: {readable:?}"
        );
        assert!(
            deny_read.iter().any(|p| mare_host::session_orchestrator::canonical_dir(std::path::Path::new(p)) == mare_host::session_orchestrator::canonical_dir(&secret_dir)),
            "session deny_read dir must survive restore under broader ceiling: {deny_read:?}"
        );
        assert!(
            deny_read.iter().any(|p| mare_host::session_orchestrator::canonical_dir(std::path::Path::new(p)) == mare_host::session_orchestrator::canonical_dir(&host_secret_dir)),
            "host read-only preset deny_read must be intersected with session policy: {deny_read:?}"
        );
        h.shutdown().await;

        // (b) Untrusted project with `untrusted_preset = "auto"` also preserves session read restrictions.
        let untrusted_cfg = json!({
            "untrusted_preset": "auto",
            "accept_partial": ["fs_protect", "fs_read", "fs_deny_read"]
        });
        let h2 = host(&env, false, untrusted_cfg, json!({})).await;
        global(&h2, "LOAD_PATH", path.to_string_lossy().to_string());
        let (p2, l2, rr2, r2_json, d2_json): (String, String, bool, String, String) = eval(
            &h2,
            r#"
            S:load({ path = LOAD_PATH })
            local cur = mare.proc.sandbox.current()
            return P:preset(),
                cur and cur.level or "none",
                cur and cur.restrict_read == true or false,
                mare.json.encode(cur and cur.readable or {}),
                mare.json.encode(cur and cur.deny_read or {})
            "#,
        )
        .await;
        assert_eq!((p2.as_str(), l2.as_str()), ("read-only", "read-only"));
        assert!(rr2, "untrusted resume must preserve session restrict_read");
        let r2: Vec<String> = serde_json::from_str(&r2_json).unwrap();
        let d2: Vec<String> = serde_json::from_str(&d2_json).unwrap();
        assert!(
            r2.iter().any(|p| mare_host::session_orchestrator::canonical_dir(std::path::Path::new(p)) == mare_host::session_orchestrator::canonical_dir(&allowed_dir)),
            "untrusted resume must preserve session readable dir: {r2:?}"
        );
        assert!(
            d2.iter().any(|p| mare_host::session_orchestrator::canonical_dir(std::path::Path::new(p)) == mare_host::session_orchestrator::canonical_dir(&secret_dir)),
            "untrusted resume must preserve session deny_read dir: {d2:?}"
        );
        h2.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn sandbox_mode_only_readonly_with_network_preserves_deny_read_under_ceiling() {
    local(async {
        let env = TestEnv::new().await;
        let secret_dir = env.cwd.join("ro-net-secret");
        std::fs::create_dir_all(&secret_dir).unwrap();
        let secret_canon = mare_host::session_orchestrator::canonical_dir(&secret_dir).to_string_lossy().into_owned();

        // Session saved a custom read-only + network=true + deny_read `sandbox/mode` record (no permission/preset).
        let path = env.write_session(
            "mode-only-ro-network.jsonl",
            &[
                json!({"type": "message", "entry_id": "m1", "parent_entry_id": null, "message": {"role": "user", "content": "hi"}}),
                json!({
                    "type": "custom",
                    "kind": "sandbox/mode",
                    "provenance": "session-host",
                    "data": {
                        "level": "read-only",
                        "deny_read": [secret_canon],
                        "network": true,
                        "accept_partial": ["fs_protect", "fs_deny_read"]
                    }
                }),
            ],
        );

        let cfg = json!({
            "preset": "auto",
            "untrusted_preset": "auto",
            "accept_partial": ["fs_protect", "fs_deny_read"]
        });
        for trusted in [true, false] {
            let h = host(&env, trusted, cfg.clone(), json!({})).await;
            global(&h, "LOAD_PATH", path.to_string_lossy().to_string());
            global(&h, "USE_CEILING", trusted);
            let (preset, level, net_open, deny_json): (String, String, bool, String) = eval(
                &h,
                r#"
                if USE_CEILING then
                    S:switch({
                        kind = "load",
                        path = LOAD_PATH,
                        restore_permission_ceiling = "auto",
                        restore_sandbox_ceiling = {
                            level = "workspace-write",
                            network = false,
                            accept_partial = { "fs_protect", "fs_deny_read" },
                        },
                    })
                else
                    S:load({ path = LOAD_PATH })
                end
                local cur = mare.proc.sandbox.current()
                return P:preset(),
                    cur and cur.level or "none",
                    cur and cur.network == true or false,
                    mare.json.encode(cur and cur.deny_read or {})
                "#,
            )
            .await;
            assert_eq!(preset, "read-only", "trusted={trusted}: read-only+network mode must stay read-only");
            assert_eq!(level, "read-only", "trusted={trusted}: sandbox level must stay read-only");
            assert!(!net_open, "trusted={trusted}: network must be narrowed fail-safe");
            let deny_read: Vec<String> = serde_json::from_str(&deny_json).unwrap();
            assert!(
                deny_read.iter().any(|p| mare_host::session_orchestrator::canonical_dir(std::path::Path::new(p))
                    == mare_host::session_orchestrator::canonical_dir(&secret_dir)),
                "trusted={trusted}: read-only+network resume must preserve session deny_read: {deny_read:?}"
            );
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn enforce_sandbox_ceiling_fails_closed_when_intersect_errors() {
    local(async {
        let env = TestEnv::new().await;

        // 1. When `intersect` fails while snapshots succeed and current policy (`read-only`) differs
        //    from `sb_ceiling` (`workspace-write`), enforce_sb_ceiling must fail closed and MUST NOT
        //    widen the active sandbox to `sb_ceiling`.
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (preset_after, level_after, tool_denied, req_decision): (String, String, bool, String) = eval(
            &h,
            r#"
            T:register("probe_intersect_fail", {
                description = "probe tool",
                parameters = { type = "object", properties = {} },
                execute = function() return { content = "ran" } end,
            })
            local orig_intersect = mare.proc.sandbox.intersect
            mare.proc.sandbox.intersect = function()
                error("opaque native profile cannot be intersected")
            end
            C.cordis:get("bus"):emit("session/new", {
                restore_permission_ceiling = "auto",
                restore_sandbox_ceiling = { level = "workspace-write", network = false },
            })
            mare.proc.sandbox.intersect = orig_intersect
            local cur = mare.proc.sandbox.current()
            local tool_res = T:execute({ id = "p-int-1", name = "probe_intersect_fail", arguments = {} })
            local tool_text = type(tool_res.content) == "string" and tool_res.content
                or mare.json.encode(tool_res.content)
            local tool_denied = tool_res.is_error == true
                and tool_text:find("opaque native profile cannot be intersected", 1, true) ~= nil
            local req = C.cordis:get("policy"):decide("request/ready", { {} })
            return P:preset(), cur and cur.level or "none", tool_denied, tostring(req and req.decision or "nil")
            "#,
        )
        .await;
        assert_eq!(preset_after, "off", "failed intersect with distinct ceiling must fail closed");
        assert_eq!(level_after, "read-only", "failed intersect must never widen active sandbox to ceiling");
        assert!(tool_denied, "failed intersect must deny subsequent tool calls");
        assert_eq!(req_decision, "deny", "failed intersect must deny subsequent request/ready");
        h.shutdown().await;

        // 2. When `intersect` fails but `snapshot_current` already matches `snapshot_policy(sb_ceiling)`,
        //    the ceiling is already satisfied and permissions stay ready.
        let h2 = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (preset_ok, req_ok): (String, String) = eval(
            &h2,
            r#"
            local cur_snap = mare.proc.sandbox.snapshot_current()
            local orig_intersect = mare.proc.sandbox.intersect
            mare.proc.sandbox.intersect = function()
                error("opaque native profile cannot be intersected")
            end
            C.cordis:get("bus"):emit("session/new", {
                restore_permission_ceiling = "read-only",
                restore_sandbox_ceiling = cur_snap,
            })
            mare.proc.sandbox.intersect = orig_intersect
            local req = C.cordis:get("policy"):decide("request/ready", { {} })
            return P:preset(), tostring(req and req.decision or "nil")
            "#,
        )
        .await;
        assert_eq!(preset_ok, "read-only", "matching snapshot must satisfy ceiling even when intersect errors");
        assert_ne!(req_ok, "deny", "matching snapshot must keep request/ready open");
        h2.shutdown().await;
    })
    .await
}
