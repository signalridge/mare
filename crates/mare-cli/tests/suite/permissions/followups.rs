//! Preset inheritance, restoration notices and trust boundaries.

use super::*;

fn custom_presets() -> Value {
    json!({
        "enabled": true,
        "untrusted_preset": "auto",
        "presets": {
            "a": { "like": "full-access", "level": "read-only" },
            "b": { "like": "a", "level": "read-only" },
            "quiet-ro": { "like": "full-access", "level": "read-only" },
            "approval-read": { "like": "read-only", "level": "danger-full-access" }
        }
    })
}

fn saved_preset(env: &TestEnv, filename: &str, preset: &str) -> std::path::PathBuf {
    env.write_session(
        filename,
        &[
            json!({"type":"message","entry_id":"m1","parent_entry_id":null,"message":{"role":"user","content":"hi"}}),
            json!({"type":"custom","kind":"sandbox/mode","provenance":"session-host","data":{"level":"read-only","network":false}}),
            json!({"type":"custom","kind":"permission/preset","provenance":"session-host","data":{"preset":preset,"source":"user"}}),
        ],
    )
}

async fn fake_ui(h: &mare_host::Host) {
    exec(
        h,
        r#"N = {}
           mare.ui.connected = function() return true end
           mare.ui.notify = function(message) N[#N + 1] = tostring(message) end"#,
    )
    .await;
}

async fn notices(h: &mare_host::Host) -> String {
    eval(h, "return table.concat(N, '\\n')").await
}

async fn load_session(h: &mare_host::Host, path: &std::path::Path) {
    global(h, "S", h.service_table("session").unwrap());
    global(h, "TARGET", path.to_string_lossy().to_string());
    exec(h, "S:load({ path = TARGET })").await;
}

fn install_child_probe(env: &TestEnv) {
    let dir = env.config_dir().join("plugins");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("permission-ceiling-probe.lua"),
        r#"return { name = "permission-ceiling-probe", inject = { "bus", "agent", "permissions" },
          contracts = { probe = { owner = "permission-ceiling-probe", events = {
            ["permission-inheritance"] = { doc = "probe", mode = "emit", expose = "both", delivery = "required", ignorable = false, data = { type = "object", additionalProperties = true }, example = {} },
          } } },
          apply = function(ctx)
            if mare.mode ~= "child" then return end
            ctx:on("turn/started", function()
              ctx.bus:emit("probe/permission-inheritance", {
                ceiling = mare.args.permission_ceiling,
                permission = mare.config.plugins and mare.config.plugins.permissions and mare.config.plugins.permissions.preset,
                preset = ctx.permissions:preset() })
            end)
          end }"#,
    )
    .unwrap();
}

/// reference implementation: preset policy must follow its declared approval behavior.
#[tokio::test]
async fn chained_like_resolution_is_order_independent_and_cycle_safe() {
    local(async {
        let env = TestEnv::new().await;
        let mut opts = custom_presets();
        opts["presets"]["loop-a"] = json!({"like":"loop-b","level":"read-only"});
        opts["presets"]["loop-b"] = json!({"like":"loop-a","level":"read-only"});
        opts["presets"]["missing"] = json!({"like":"not-defined","level":"read-only"});
        opts["presets"]["plain"] = json!({"level":"read-only"});
        opts["presets"]["read-only"] = json!({"label":"READ OVERRIDE"});
        let h = host(&env, true, opts, json!({"no_session":true})).await;
        let (a, b, cycle_a, cycle_b, missing, builtin, plain): (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
        ) = eval(
            &h,
            r#"return P:describe("a").like, P:describe("b").like,
                 P:describe("loop-a").like, P:describe("loop-b").like,
                 P:describe("missing").like, P:describe("read-only").like, P:describe("plain").like"#,
        )
        .await;
        assert_eq!((a.as_str(), b.as_str()), ("full-access", "full-access"));
        assert_eq!((cycle_a.as_str(), cycle_b.as_str(), missing.as_str()), ("read-only", "read-only", "read-only"));
        assert_eq!(builtin, "read-only", "a builtin override without like keeps its canonical behavior");
        assert_eq!(plain, "auto", "a new preset without like defaults to auto approval");
        h.shutdown().await;
    })
    .await;
}

/// reference implementation: restored policy is enforced; Kimi runtime.test.ts:766 checks resume state.
#[tokio::test]
async fn chained_full_access_is_capped_for_untrusted_and_child() {
    local(async {
        let env = TestEnv::new().await;
        let path = saved_preset(&env, "chain-resume.jsonl", "b");
        let h = host(&env, false, custom_presets(), json!({})).await;
        load_session(&h, &path).await;
        let got: String = eval(&h, "return P:preset()").await;
        assert_eq!(got, "auto", "approval behavior must be capped on untrusted load");
        h.shutdown().await;

        let mut child_cfg = custom_presets();
        child_cfg["preset"] = json!("auto");
        let mut b = env
            .host()
            .config(json!({"plugins":{"permissions":child_cfg}}))
            .args(json!({"no_session":true,"permission_ceiling":"auto"}));
        b.opts.mode = Mode::Child;
        b.opts.project_trusted = true;
        let child = b.build().await;
        global(&child, "P", child.service_table("permissions").unwrap());
        let (rejected, err): (bool, String) = eval(
            &child,
            r#"local ok, e = pcall(P.set, P, "b"); return not ok, type(e) == "table" and tostring(e.message) or tostring(e)"#,
        )
        .await;
        assert!(rejected && err.contains("can't widen"), "{err}");
        child.shutdown().await;
    })
    .await;
}

/// Codex session_lifecycle_requests.rs:6063: a real downgrade must be visible to the user.
#[tokio::test]
async fn capped_restores_report_full_and_custom_preset_warnings() {
    local(async {
        let env = TestEnv::new().await;
        for preset in ["full-access", "quiet-ro"] {
            let path = saved_preset(&env, &format!("warning-{preset}.jsonl"), preset);
            let h = host(&env, false, custom_presets(), json!({})).await;
            fake_ui(&h).await;
            load_session(&h, &path).await;
            let actual: String = eval(&h, "return P:preset()").await;
            let log = notices(&h).await;
            assert_eq!(actual, "auto");
            assert!(log.contains(&format!("capped restored permission preset from {preset} to auto")), "{log}");
            h.shutdown().await;
        }
    })
    .await;
}

#[tokio::test]
async fn legacy_network_warning_only_reports_actual_narrowing() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "legacy-network.jsonl",
            &[
                json!({"type":"message","entry_id":"m1","parent_entry_id":null,"message":{"role":"user","content":"hi"}}),
                json!({"type":"custom","kind":"sandbox/mode","provenance":"session-host","data":{"level":"workspace-write","network":true}}),
            ],
        );
        for (cfg, expected, network, warn) in [
            (json!({"untrusted_preset":"auto","network":true}), "auto", true, false),
            (json!({"untrusted_preset":"strict-net","presets":{"strict-net":{"like":"auto","level":"workspace-write","network":false}}}), "strict-net", false, true),
        ] {
            let h = host(&env, false, cfg, json!({})).await;
            fake_ui(&h).await;
            load_session(&h, &path).await;
            let (preset, actual_network): (String, bool) =
                eval(&h, "local s=mare.proc.sandbox.current(); return P:preset(), s.network == true").await;
            let log = notices(&h).await;
            assert_eq!((preset.as_str(), actual_network), (expected, network));
            assert_eq!(log.contains("capped restored permission preset"), warn, "{expected}: {log}");
            h.shutdown().await;
        }
    })
    .await;
}

#[tokio::test]
async fn failed_capped_restore_does_not_report_successful_downgrade() {
    local(async {
        let env = TestEnv::new().await;
        let path = saved_preset(&env, "failed-capped-restore.jsonl", "quiet-ro");
        let h = host(&env, false, custom_presets(), json!({})).await;
        fake_ui(&h).await;
        global(&h, "S", h.service_table("session").unwrap());
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let ok: bool = eval(
            &h,
            r#"S.set_sandbox_mode = function() return nil, "injected setter failure" end
               return pcall(S.load, S, {path=TARGET})"#,
        )
        .await;
        assert!(ok, "the session load itself completes despite refusing its permission restore");
        let log = notices(&h).await;
        assert!(log.contains("injected setter failure"), "{log}");
        assert!(!log.contains("capped restored permission preset"), "must not announce an unapplied cap: {log}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn quiet_ro_remains_capped_across_fork_and_permissions_reload() {
    local(async {
        let env = TestEnv::new().await;
        let path = saved_preset(&env, "quiet-fork.jsonl", "quiet-ro");
        let h = host(&env, false, custom_presets(), json!({})).await;
        fake_ui(&h).await;
        load_session(&h, &path).await;
        let turn = h.prompt("complete a turn before forking", json!({})).await.unwrap();
        assert!(turn["error"].is_null(), "{turn}");
        let (forked, fork_preset, fork_records, fork_notice, reload_preset, reload_notice): (
            bool,
            String,
            String,
            String,
            String,
            String,
        ) = eval(
            &h,
            r#"local before = S:info().session_id
               N = {}
               S:fork({session_id=before})
               local forked = S:info().session_id ~= before
               local fork_preset = C.cordis:get("permissions"):preset()
               local rs = S:list_records("permission/preset", { branch = "current" }).data
               local record = rs[#rs] and rs[#rs].preset or ""
               local fork_notice = table.concat(N, "\n")
               N = {}
               local kernel=C.cordis:get("kernel")
               kernel:reload("permissions"); kernel:reload_barrier()
               return forked, fork_preset, record, fork_notice,
                   C.cordis:get("permissions"):preset(), table.concat(N, "\n")"#,
        )
        .await;
        assert!(forked);
        assert_eq!(fork_records, "quiet-ro", "fork must actually restore the saved risky preset");
        assert_eq!((fork_preset.as_str(), reload_preset.as_str()), ("auto", "auto"));
        assert!(fork_notice.contains("capped restored permission preset"), "{fork_notice}");
        assert!(reload_notice.contains("capped restored permission preset"), "{reload_notice}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn revoking_trust_caps_an_active_quiet_ro_preset() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, custom_presets(), json!({})).await;
        fake_ui(&h).await;
        let turn = h.prompt("complete a turn before revoking trust", json!({})).await.unwrap();
        assert!(turn["error"].is_null(), "{turn}");
        exec(&h, r#"assert(P:set("quiet-ro")); assert(P:preset() == "quiet-ro"); N = {}"#).await;
        let saved: String = eval(
            &h,
            r#"local s = C.cordis:get("session"); local r = s:list_records("permission/preset", {branch="current"}).data
               return r[#r] and r[#r].preset or """#,
        )
        .await;
        assert_eq!(saved, "quiet-ro", "the active session must record the risky preset before revocation");
        let workspaces = mare_host::WorkspaceRegistry::new();
        workspaces.attach("parent", &h);
        let id = h.workspace.id().clone();
        assert!(workspaces.set_trust(&id, false, false).await.unwrap());
        let (trusted, preset): (bool, String) =
            eval(&h, "return mare.trusted, C.cordis:get('permissions'):preset()").await;
        assert!(!trusted);
        assert_eq!(preset, "auto");
        let log = notices(&h).await;
        assert!(log.contains("capped restored permission preset from quiet-ro to auto"), "{log}");
        h.shutdown().await;
    })
    .await;
}

/// Exercise the host's inheritance by observing the actual child VM, not a simulated child.
#[tokio::test]
async fn child_spawn_cannot_widen_approval_rank_with_narrower_sandbox() {
    local(async {
        let env = TestEnv::new().await;
        install_child_probe(&env);
        let mut builder = env
            .host()
            .user_plugins()
            .faux(json!([{"text":"done"}]))
            .config(json!({
                "plugins":{"permissions":custom_presets()}
            }))
            .args(json!({"no_session":true}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        let (ceiling, permission, preset): (String, String, String) = eval(
            &h,
            r#"assert(C.cordis:get("permissions"):set("auto"))
               local child = mare.subagent.start({prompt="check ceiling", no_session=true,
                   permission_ceiling="quiet-ro", permission="quiet-ro"})
               local seen
               while true do
                 local ev = child:next_event()
                 if not ev then break end
                 if ev.name == "probe/permission-inheritance" then seen = ev.payload; break end
               end
               local done = child:wait()
               assert(seen, "missing child permission probe: " .. mare.json.encode(done))
               return seen.ceiling, seen.permission, seen.preset"#,
        )
        .await;
        assert_eq!((ceiling.as_str(), permission.as_str(), preset.as_str()), ("auto", "auto", "auto"));
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn child_permission_is_checked_independently_of_its_ceiling() {
    local(async {
        let env = TestEnv::new().await;
        install_child_probe(&env);
        let mut builder = env
            .host()
            .user_plugins()
            .faux(json!([{"text":"done"}]))
            .config(json!({
                "plugins":{"permissions":custom_presets()}
            }))
            .args(json!({"no_session":true}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        let (bad_ceiling, bad_permission, good_ceiling, good_permission): (String, String, String, String) = eval(
            &h,
            r#"assert(C.cordis:get("permissions"):set("auto"))
               local function observe(permission)
                 local child = mare.subagent.start({prompt="check permission", no_session=true,
                     permission_ceiling="auto", permission=permission})
                 local seen
                 while true do
                   local ev = child:next_event()
                   if not ev then break end
                   if ev.name == "probe/permission-inheritance" then seen = ev.payload; break end
                 end
                 local done = child:wait()
                 assert(seen, "missing child permission probe: " .. mare.json.encode(done))
                 return seen.ceiling, seen.permission
               end
               local a, b = observe("quiet-ro")
               local c, d = observe("read-only")
               return a, b, c, d"#,
        )
        .await;
        assert_eq!((bad_ceiling.as_str(), bad_permission.as_str()), ("auto", "auto"));
        assert_eq!((good_ceiling.as_str(), good_permission.as_str()), ("auto", "read-only"));
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn child_uses_sandbox_rank_when_describe_omits_like() {
    local(async {
        let env = TestEnv::new().await;
        install_child_probe(&env);
        let mut builder = env
            .host()
            .user_plugins()
            .faux(json!([{"text":"done"}]))
            .config(json!({
                "plugins":{"permissions":custom_presets()}
            }))
            .args(json!({"no_session":true}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        let (ceiling, permission): (String, String) = eval(
            &h,
            r#"local permissions = C.cordis:get("permissions")
               assert(permissions:set("auto"))
               local describe = permissions.describe
               permissions.describe = function(self, name)
                 local d = describe(self, name)
                 if name == "read-only" then d.like = nil end
                 return d
               end
               local child = mare.subagent.start({prompt="check missing like", no_session=true,
                   permission_ceiling="auto", permission="read-only"})
               local seen
               while true do
                 local ev = child:next_event()
                 if not ev then break end
                 if ev.name == "probe/permission-inheritance" then seen = ev.payload; break end
               end
               local done = child:wait()
               assert(seen, "missing child permission probe: " .. mare.json.encode(done))
               return seen.ceiling, seen.permission"#,
        )
        .await;
        assert_eq!((ceiling.as_str(), permission.as_str()), ("auto", "read-only"));
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn child_inherits_parent_approval_when_active_sandbox_is_narrower() {
    local(async {
        let env = TestEnv::new().await;
        install_child_probe(&env);
        let mut builder = env
            .host()
            .user_plugins()
            .faux(json!([{"text":"done"}]))
            .config(json!({
                "plugins":{"permissions":custom_presets()}
            }))
            .args(json!({"no_session":true}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        let (ceiling, permission, preset): (String, String, String) = eval(
            &h,
            r#"assert(C.cordis:get("permissions"):set("approval-read"))
               local child = mare.exec.run({cwd=mare.cwd, sandbox={level="workspace-write", network=true,
                   accept_partial={"fs_protect"}}}, function()
                 return mare.subagent.start({prompt="check inherited approval", no_session=true,
                     permission_ceiling="auto", permission="auto"})
               end)
               local seen
               while true do
                 local ev = child:next_event()
                 if not ev then break end
                 if ev.name == "probe/permission-inheritance" then seen = ev.payload; break end
               end
               local done = child:wait()
               assert(seen, "missing child permission probe: " .. mare.json.encode(done))
               return seen.ceiling, seen.permission, seen.preset"#,
        )
        .await;
        assert_eq!((ceiling.as_str(), permission.as_str(), preset.as_str()), ("read-only", "read-only", "read-only"));
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn child_preserves_the_parent_preset_when_both_ranks_fit() {
    local(async {
        let env = TestEnv::new().await;
        install_child_probe(&env);
        let mut builder = env
            .host()
            .user_plugins()
            .faux(json!([{"text":"done"}]))
            .config(json!({
                "plugins":{"permissions":custom_presets()}
            }))
            .args(json!({"no_session":true}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        let (ceiling, permission, preset): (String, String, String) = eval(
            &h,
            r#"assert(C.cordis:get("permissions"):set("quiet-ro"))
               local child = mare.subagent.start({prompt="inherit quiet-ro", no_session=true})
               local seen
               while true do
                 local ev = child:next_event()
                 if not ev then break end
                 if ev.name == "probe/permission-inheritance" then seen = ev.payload; break end
               end
               local done = child:wait()
               assert(seen, "missing child permission probe: " .. mare.json.encode(done))
               return seen.ceiling, seen.permission, seen.preset"#,
        )
        .await;
        assert_eq!((ceiling.as_str(), permission.as_str(), preset.as_str()), ("quiet-ro", "quiet-ro", "quiet-ro"));
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn child_spawn_refuses_when_no_preset_meets_both_parent_limits() {
    local(async {
        let env = TestEnv::new().await;
        let mut opts = custom_presets();
        opts["presets"]["read-only"] = json!({"like":"full-access","level":"read-only"});
        opts["presets"]["auto"] = json!({"like":"full-access","level":"workspace-write"});
        let h = host(&env, true, opts, json!({"no_session":true})).await;
        let (ok, error): (bool, String) = eval(
            &h,
            r#"assert(P:set("approval-read"))
               local ok, result = pcall(function()
                 return mare.exec.run({cwd=mare.cwd, sandbox={level="read-only", network=false,
                     accept_partial={"fs_protect"}}}, function()
                   local child = mare.subagent.start({prompt="must refuse", no_session=true,
                       permission_ceiling="read-only", permission="read-only"})
                   return child:wait()
                 end)
               end)
               return ok, tostring(result)"#,
        )
        .await;
        assert!(!ok && error.contains("no permission preset satisfies the inherited ceiling"), "{error}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn child_uses_a_safe_custom_fallback_when_builtins_are_wider() {
    local(async {
        let env = TestEnv::new().await;
        install_child_probe(&env);
        let mut opts = custom_presets();
        opts["presets"]["read-only"] = json!({"like":"full-access","level":"read-only"});
        opts["presets"]["auto"] = json!({"like":"full-access","level":"workspace-write"});
        opts["presets"]["safe-fallback"] = json!({"like":"read-only","level":"read-only"});
        let mut builder = env
            .host()
            .user_plugins()
            .faux(json!([{"text":"done"}]))
            .config(json!({
                "plugins":{"permissions":opts}
            }))
            .args(json!({"no_session":true}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        let (ceiling, permission, preset): (String, String, String) = eval(
            &h,
            r#"assert(C.cordis:get("permissions"):set("approval-read"))
               local child = mare.exec.run({cwd=mare.cwd, sandbox={level="read-only", network=false,
                   accept_partial={"fs_protect"}}}, function()
                 return mare.subagent.start({prompt="choose safe fallback", no_session=true,
                     permission_ceiling="auto", permission="auto"})
               end)
               local seen
               while true do
                 local ev = child:next_event()
                 if not ev then break end
                 if ev.name == "probe/permission-inheritance" then seen = ev.payload; break end
               end
               local done = child:wait()
               assert(seen, "missing child permission probe: " .. mare.json.encode(done))
               return seen.ceiling, seen.permission, seen.preset"#,
        )
        .await;
        assert_eq!(
            (ceiling.as_str(), permission.as_str(), preset.as_str()),
            ("safe-fallback", "safe-fallback", "safe-fallback")
        );
        h.shutdown().await;
    })
    .await;
}
