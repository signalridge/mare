use super::*;

#[tokio::test]
async fn readonly_resume_restores_permission_preset_and_keeps_tools_usable() {
    local(async {
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(env.cwd.join("a.txt"), "read-only restore marker").unwrap();
        let path = env.write_session("readonly-permission.jsonl", &[
            msg("m1", None, "user", "HISTORY"),
            json!({"type":"custom", "kind":"permission/preset", "provenance":"session-host", "data":{"preset":"read-only", "source":"user"}}),
        ]);
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&dunce::canonicalize(elsewhere.path()).unwrap()).unwrap(), 1);
        std::fs::write(&path, &body).unwrap();
        let mut builder = env.host().faux(json!([
            {"tool_calls":[{"name":"read", "arguments":{"path":"a.txt"}}]},
            {"text":"tool round finished"}
        ])).args(json!({"trust":"trusted"})).config(json!({"plugins":{"permissions":{"preset":"full-access","enabled":true}}}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (readonly, level, preset): (bool, String, String) = eval(&h, r#"
            mare.ui.select = function() return 3 end
            S:load({path=TARGET})
            return S:info().readonly == true, mare.proc.sandbox.current().level, C.cordis:get('permissions'):preset()
        "#).await;
        assert!(readonly);
        assert_eq!((level.as_str(), preset.as_str()), ("read-only", "read-only"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body, "restore must not write the read-only session");
        let result = h.prompt("read the file", json!({})).await.unwrap();
        assert_eq!(result["text"], "tool round finished", "{result}");
        let messages: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert!(messages.contains("read-only restore marker"), "read tool result must reach the conversation: {messages}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn missing_sandbox_root_does_not_block_resume() {
    local(async {
        let env = TestEnv::new().await;
        let missing = env.cwd.join("missing-root");
        let kept = env.cwd.join("kept-root");
        std::fs::create_dir(&kept).unwrap();
        let path = env.write_session("missing-root.jsonl", &[msg("m1", None, "user", "TARGET"), json!({"type":"custom", "kind":"sandbox/mode", "provenance":"session-host", "data":{"level":"workspace-write", "writable":[missing, kept]}})]);
        let h = env.host().faux(json!([{ "text": "OK" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, switching): (bool, String, bool) = eval(&h, r#"
            local ok, err = pcall(S.load, S, { path = TARGET })
            return ok, tostring(err), S:switching()
        "#).await;
        assert!(ok && !switching, "{err}");
        assert!(notices(&h).await.contains("no longer exists"));
        assert_eq!(eval::<usize>(&h, "return #mare.proc.sandbox.current().writable").await, 1);
        let entries = std::fs::read_to_string(&path).unwrap();
        assert_eq!(entries.lines().filter(|l| l.contains("sandbox/mode")).count(), 1);
        assert_eq!(h.prompt("NEXT", json!({})).await.unwrap()["text"], "OK");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn model_setter_failures_warn_and_keep_the_session_usable() {
    local(async {
        for mode in ["throw", "nil", "false"] {
            let env = TestEnv::new().await;
            let path = env.write_session("model-failure.jsonl", &[msg("m1", None, "user", "TARGET"), json!({"type":"model", "model":"faux/echo", "at_entry_id":"m1"})]);
            let h = env.host().faux(json!([{ "text": "OK" }])).build().await;
            expose(&h);
            fake_ui(&h).await;
            global(&h, "L", h.service_table("llm").unwrap());
            global(&h, "TARGET", path.to_string_lossy().to_string());
            global(&h, "FAIL_MODE", mode);
            let (ok, err, switching, events): (bool, String, bool, String) = eval(&h, r#"
                EVENTS = {}
                C.cordis:on('agent/messages-replaced', function() EVENTS[#EVENTS + 1] = 'messages' end)
                C.cordis:on('session/changed', function(e) if e.reason == 'resume' then EVENTS[#EVENTS + 1] = 'changed' end end)
                C.cordis:on('session/loaded', function() EVENTS[#EVENTS + 1] = 'loaded' end)
                -- resume restores through restore_selection, not the public setter
                local original = L.restore_selection
                L.restore_selection = function(self, sel)
                    if sel and sel.model then
                        if FAIL_MODE == 'nil' then return nil, 'injected model failure' end
                        if FAIL_MODE == 'false' then return false, 'injected model failure' end
                        error('injected model failure')
                    end
                    return original(self, sel)
                end
                local ok, err = pcall(S.load, S, { path = TARGET })
                L.restore_selection = original
                return ok, tostring(err), S:switching(), table.concat(EVENTS, ',')
            "#).await;
            assert!(ok && !switching, "{mode}: {err}");
            assert_eq!(events, "messages,changed,loaded");
            assert!(notices(&h).await.contains("injected model failure"));
            assert_eq!(eval::<String>(&h, "return L:current().ref").await, "faux/echo");
            assert_eq!(h.prompt("NEXT", json!({})).await.unwrap()["text"], "OK");
            h.shutdown().await;
        }
    }).await
}

#[tokio::test]
async fn effort_setter_failures_warn_and_restore_the_model_default() {
    local(async {
        for mode in ["throw", "nil", "false"] {
            let env = TestEnv::new().await;
            let path = env.write_session(
                "effort-failure.jsonl",
                &[
                    msg("m1", None, "user", "TARGET"),
                    json!({"type":"model", "model":"faux/echo", "effort":"high", "at_entry_id":"m1"}),
                ],
            );
            let h = env.host().faux(json!([{ "text": "OK" }])).build().await;
            expose(&h);
            fake_ui(&h).await;
            global(&h, "L", h.service_table("llm").unwrap());
            global(&h, "TARGET", path.to_string_lossy().to_string());
            global(&h, "FAIL_MODE", mode);
            let default_effort: Option<String> = eval(&h, "return L:raw_effort()").await;
            exec(&h, r#"L:set_effort("low", {save=false})"#).await;
            let (ok, err, switching, model): (bool, String, bool, String) = eval(
                &h,
                r#"
                local setter = L.restore_selection
                L.restore_selection = function(self, sel)
                    if sel and sel.effort then
                        if FAIL_MODE == 'nil' then return nil, 'injected effort failure' end
                        if FAIL_MODE == 'false' then return false, 'injected effort failure' end
                        error('injected effort failure')
                    end
                    return setter(self, sel)
                end
                local ok, err = pcall(S.load, S, { path = TARGET })
                L.restore_selection = setter
                return ok, tostring(err), S:switching(), L:current().ref
            "#,
            )
            .await;
            assert!(ok && !switching, "{mode}: {err}");
            assert_eq!(model, "faux/echo");
            assert!(notices(&h).await.contains("injected effort failure"));
            assert_eq!(eval::<Option<String>>(&h, "return L:raw_effort()").await, default_effort);
            assert_eq!(h.prompt("NEXT", json!({})).await.unwrap()["text"], "OK");
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn unavailable_model_without_a_default_keeps_session_open_and_refuses_the_turn() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "no-model-default.jsonl",
            &[
                msg("m1", None, "user", "TARGET"),
                json!({"type":"model", "model":"missing-provider/model", "effort":"invalid", "at_entry_id":"m1"}),
            ],
        );
        // No usable default: the configured one names a provider that doesn't exist (a provider
        // needing no credentials, like the default faux, would count as available).
        let h = env.host().config(json!({ "model": "missing-default/model" })).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, switching, model_selected): (bool, String, bool, bool) = eval(
            &h,
            r#"
            local ok, err = pcall(S.load, S, { path = TARGET })
            return ok, tostring(err), S:switching(), L:current() ~= nil
        "#,
        )
        .await;
        assert!(ok && !switching && !model_selected, "{err}");
        let notices = notices(&h).await;
        assert!(notices.contains("session model not restored"), "{notices}");
        assert!(notices.contains("no available default model"), "{notices}");
        let request = h.prompt("NEXT", json!({})).await.unwrap();
        assert_eq!(request["not_sent"], true, "{request}");
        assert!(request["error"]["message"].as_str().unwrap().contains("no model"), "{request}");
        assert!(!request["error"]["message"].as_str().unwrap().contains("session committed"), "{request}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn enabled_permissions_default_survives_resume_and_new_without_session_policy_records() {
    local(async {
        for (preset, expected) in [("auto", "workspace-write"), ("full-access", "")] {
            let env = TestEnv::new().await;
            let path = env.write_session("permissions-default.jsonl", &[msg("m1", None, "user", "HISTORY")]);
            let mut builder = env
                .host()
                .faux(json!([{ "text": "OK" }]))
                .args(json!({"trust":"trusted"}))
                .config(json!({"plugins":{"permissions":{"preset":preset,"enabled":true}}}));
            builder.opts.project_trusted = true;
            let h = builder.build().await;
            expose(&h);
            global(&h, "TARGET", path.to_string_lossy().to_string());
            let initial: String = eval(&h, "local p=mare.proc.sandbox.current(); return p and p.level or ''").await;
            assert_eq!(initial, expected, "the enabled {preset} default is active at startup");
            let (ok, err, switching, resumed): (bool, String, bool, String) = eval(
                &h,
                r#"
                local ok, err = pcall(S.load, S, { path = TARGET })
                local p = mare.proc.sandbox.current()
                return ok, tostring(err), S:switching(), p and p.level or ''
            "#,
            )
            .await;
            assert!(ok && !switching, "{preset}: {err}");
            assert_eq!(resumed, expected, "a session without policy records keeps the configured default");

            exec(&h, r#"S:set_sandbox_mode('read-only')"#).await;
            assert_eq!(eval::<String>(&h, "return mare.proc.sandbox.current().level").await, "read-only");
            exec(&h, r#"S:create()"#).await;
            let new_session: String = eval(&h, "local p=mare.proc.sandbox.current(); return p and p.level or ''").await;
            assert_eq!(new_session, expected, "new restores {preset} after a session override");
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn failed_restored_permission_preset_blocks_prompts_and_preserves_the_session_record() {
    local(async {
        for preset in ["read-only", "auto"] {
            let env = TestEnv::new().await;
            let path = env.write_session(
                "permission-restore-noop.jsonl",
                &[
                    msg("m1", None, "user", "HISTORY"),
                    json!({"type":"custom", "kind":"permission/preset", "provenance":"session-host", "data":{"preset":preset, "source":"user"}}),
                ],
            );
            let mut builder = env
                .host()
                .faux(json!([{ "text": "MUST NOT BE SENT" }]))
                .args(json!({"trust":"trusted"}))
                .config(json!({"plugins":{"permissions":{"preset":"full-access","enabled":true}}}));
            builder.opts.project_trusted = true;
            let h = builder.build().await;
            expose(&h);
            fake_ui(&h).await;
            global(&h, "S", h.service_table("session").unwrap());
            global(&h, "P", h.service_table("permissions").unwrap());
            global(&h, "TARGET", path.to_string_lossy().to_string());
            let (ok, err, current_is_full, active_preset): (bool, String, bool, String) = eval(
                &h,
                r#"
                S.set_sandbox_mode = function() return nil end
                local ok, err = pcall(S.load, S, { path = TARGET })
                return ok, tostring(err), mare.proc.sandbox.current() == nil, P:preset()
            "#,
            )
            .await;
            assert!(ok, "session itself should load: {err}");
            assert!(current_is_full, "fixture must expose the no-op setter hazard");
            assert_eq!(active_preset, "off", "a failed restore must not claim a preset is active");
            let request = h.prompt("NEXT", json!({})).await.unwrap();
            assert_eq!(request["not_sent"], true, "unrestored {preset} must not run a prompt: {request}");
            assert!(
                request["error"]["message"].as_str().unwrap_or_default().contains("permission sandbox could not be restored"),
                "the refusal needs a visible reason: {request}"
            );
            assert!(
                notices(&h).await.contains("permission sandbox could not be restored"),
                "notify the user when the persisted preset cannot be applied"
            );
            let (count, saved): (usize, String) =
                eval(&h, "local rs=S:list_records('permission/preset').data; return #rs, rs[#rs].preset").await;
            assert_eq!(
                (count, saved.as_str()),
                (1, preset),
                "failed restoration must preserve saved permission intent"
            );
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn replacement_session_setter_restores_saved_read_and_auto_presets() {
    local(async {
        for (preset, expected_level) in [("read-only", "read-only"), ("auto", "workspace-write")] {
            let env = TestEnv::new().await;
            let path = env.write_session(
                "permission-restore-replacement.jsonl",
                &[
                    msg("m1", None, "user", "HISTORY"),
                    json!({"type":"custom", "kind":"permission/preset", "provenance":"session-host", "data":{"preset":preset, "source":"user"}}),
                ],
            );
            let mut builder = env
                .host()
                .faux(json!([{ "text": "prompt completed" }]))
                .args(json!({"trust":"trusted"}))
                .config(json!({"plugins":{"permissions":{"preset":"full-access","enabled":true}}}));
            builder.opts.project_trusted = true;
            let h = builder.build().await;
            expose(&h);
            fake_ui(&h).await;
            global(&h, "S", h.service_table("session").unwrap());
            global(&h, "P", h.service_table("permissions").unwrap());
            global(&h, "TARGET", path.to_string_lossy().to_string());
            let (ok, err, level, active_preset): (bool, String, String, String) = eval(
                &h,
                r#"
                local host_set = mare.proc.sandbox.set_current
                S.set_sandbox_mode = function(_, spec) return host_set(spec) end
                local ok, err = pcall(S.load, S, { path = TARGET })
                local current = mare.proc.sandbox.current()
                return ok, tostring(err), current and current.level or '', P:preset()
            "#,
            )
            .await;
            assert!(ok, "a replacement implementing set_sandbox_mode must restore {preset}: {err}");
            assert_eq!(level, expected_level);
            assert_eq!(active_preset, preset);
            let request = h.prompt("NEXT", json!({})).await.unwrap();
            assert_eq!(request["text"], "prompt completed", "a valid replacement remains usable: {request}");
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn runtime_replacement_setter_mutation_restores_the_host_policy_and_blocks_prompts() {
    local(async {
        let env = TestEnv::new().await;
        let mut builder = env
            .host()
            .faux(json!([{ "text": "MUST NOT BE SENT" }]))
            .args(json!({"trust":"trusted"}))
            .config(json!({"plugins":{"permissions":{"preset":"auto","enabled":true}}}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "S", h.service_table("session").unwrap());
        global(&h, "P", h.service_table("permissions").unwrap());
        let (ok, err, level, preset): (bool, String, String, String) = eval(
            &h,
            r#"
            local host_set = mare.proc.sandbox.set_current
            S.set_sandbox_mode = function(_, _, _)
                host_set(false)
                return nil
            end
            local ok, err = pcall(P.set, P, 'read-only')
            local current = mare.proc.sandbox.current()
            return ok, tostring(err), current and current.level or '', P:preset()
        "#,
        )
        .await;
        assert!(!ok, "the replacement setter did not apply READ; this transition must fail: {err}");
        assert_eq!(level, "workspace-write", "the prior host policy must be restored after the bad mutation");
        assert_eq!(preset, "off", "failed switching must not claim READ or the prior preset is active");
        let request = h.prompt("NEXT", json!({})).await.unwrap();
        assert_eq!(request["not_sent"], true, "failed setter must block the next prompt: {request}");
        assert!(
            request["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("permission sandbox could not be restored"),
            "the blocked request needs a user-visible reason: {request}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn invalid_existing_sandbox_root_warns_and_falls_back_to_the_base_policy() {
    local(async {
        let env = TestEnv::new().await;
        let bad = env.cwd.join("not-a-directory");
        std::fs::write(&bad, "file").unwrap();
        let path = env.write_session(
            "bad-root.jsonl",
            &[
                msg("m1", None, "user", "TARGET"),
                json!({"type":"custom", "kind":"sandbox/mode", "provenance":"session-host", "data":{"level":"workspace-write", "writable":[bad]}}),
            ],
        );
        let mut builder = env
            .host()
            .faux(json!([{ "text": "OK" }]))
            .args(json!({"trust":"trusted"}))
            .config(json!({"plugins":{"permissions":{"preset":"auto","enabled":true}}}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, switching, level): (bool, String, bool, String) = eval(
            &h,
            r#"
            local ok, err = pcall(S.load, S, { path = TARGET })
            local p = mare.proc.sandbox.current()
            return ok, tostring(err), S:switching(), p and p.level or ''
        "#,
        )
        .await;
        assert!(ok && !switching, "{err}");
        assert_eq!(level, "workspace-write", "invalid session roots fall back to the enabled permissions preset");
        assert!(notices(&h).await.contains("sandbox mode not restored"));
        assert_eq!(h.prompt("NEXT", json!({})).await.unwrap()["text"], "OK");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn resume_setter_failure_after_mutation_must_poison() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "setter-after-mutation.jsonl",
            &[msg("m1", None, "user", "TARGET"), json!({"type":"model", "model":"faux/echo2", "at_entry_id":"m1"})],
        );
        let h = env.host().faux(json!([{ "text": "OK" }])).build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, switching): (bool, String, bool) = eval(
            &h,
            r#"
            local setter = L.restore_selection
            L.restore_selection = function(self, sel)
                setter(self, sel)
                if sel and sel.model then
                    error('setter failed after mutation')
                end
            end
            local ok, err = pcall(S.load, S, { path = TARGET })
            L.restore_selection = setter
            return ok, tostring(err), S:switching()
        "#,
        )
        .await;
        assert!(!ok && switching && err.contains("setter failed after mutation"), "{err}");
        assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn unavailable_record_restores_default_alias_pinned_effort() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("default-alias.jsonl", &[msg("m1", None, "user", "TARGET"), json!({"type":"model", "model":"missing-provider/model", "at_entry_id":"m1"})]);
        let h = env.host().config(json!({"model":"fast", "model_aliases":{"fast":"faux/echo"}, "model_alias_params":{"fast":{"effort":"low"}}, "plugins":{"provider-faux":{"script_json":json!([{ "text":"OK" }]).to_string()}}})).build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(&h, r#"L:set_model("faux/echo", {save=false}); L:set_effort("high", {save=false}); S:load({ path = TARGET })"#).await;
        assert_eq!(eval::<String>(&h, "return L:effort()").await, "low");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn fallback_must_not_select_unavailable_provider_model() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "unavailable-provider.jsonl",
            &[msg("m1", None, "user", "TARGET"), json!({"type":"model", "model":"faux/echo", "at_entry_id":"m1"})],
        );
        // the current model is one no provider knows (configured, never resolvable), so current()
        // has to fall back; the faux provider (configured, with a default) reports that default
        // unavailable at the provider layer (conditions are set on providers, not by
        // patching the llm service's own methods)
        let h = env.host().faux(json!([{ "text":"OK" }])).config(json!({ "model": "gone/x" })).build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (unresolved, has_default, configured, selected): (bool, bool, bool, bool) = eval(
            &h,
            r#"
            local p = L:provider('faux')
            p.unavailable_for = function() return 'route disabled' end
            return L:resolve('gone/x') == nil, p.default_model ~= nil, not p.is_configured or p.is_configured(),
                L:current() ~= nil
        "#,
        )
        .await;
        assert!(unresolved, "fixture: the configured current model must not resolve");
        assert!(has_default && configured, "fixture must have a configured default model");
        assert!(!selected, "current() selected a configured provider's unavailable default");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn missing_root_beneath_symlink_is_not_silently_omitted() {
    local(async {
        let env = TestEnv::new().await;
        let real = env.cwd.join("real");
        std::fs::create_dir(&real).unwrap();
        let child = real.join("child");
        std::fs::create_dir(&child).unwrap();
        let link = env.cwd.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let path = env.write_session("symlink-root.jsonl", &[msg("m1", None, "user", "TARGET"), json!({"type":"custom", "kind":"sandbox/mode", "provenance":"session-host", "data":{"level":"workspace-write", "writable":[link.join("child").join("missing")]}})]);
        let h = env.host().faux(json!([{ "text": "OK" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, switching, level): (bool, String, bool, String) = eval(&h, r#"
            mare.proc.sandbox.set_current({level='read-only'})
            local ok, err = pcall(S.load, S, { path = TARGET })
            local p = mare.proc.sandbox.current()
            return ok, tostring(err), S:switching(), p and p.level or ''
        "#).await;
        assert!(ok && !switching, "{err}");
        assert_eq!(level, "read-only");
        assert!(notices(&h).await.contains("sandbox mode not restored"));
        assert_eq!(h.prompt("NEXT", json!({})).await.unwrap()["text"], "OK");
        h.shutdown().await;
    }).await
}

#[cfg(unix)]
#[tokio::test]
async fn non_directory_sandbox_ancestor_falls_back_to_the_base_policy() {
    local(async {
        let env = TestEnv::new().await;
        let parent = env.cwd.join("file-parent");
        std::fs::write(&parent, "not a directory").unwrap();
        let root = parent.join("root");
        let path = env.write_session(
            "invalid-root-ancestor.jsonl",
            &[
                msg("m1", None, "user", "TARGET"),
                json!({"type":"custom", "kind":"sandbox/mode", "provenance":"session-host", "data":{"level":"workspace-write", "writable":[root]}}),
            ],
        );
        let h = env.host().faux(json!([{ "text": "OK" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, switching, level): (bool, String, bool, String) = eval(
            &h,
            "mare.proc.sandbox.set_current({level='read-only'}); local ok, err = pcall(S.load, S, { path = TARGET }); local p = mare.proc.sandbox.current(); return ok, tostring(err), S:switching(), p and p.level or ''",
        )
        .await;
        assert!(ok && !switching, "{err}");
        assert_eq!(level, "read-only");
        assert!(notices(&h).await.contains("sandbox mode not restored"));
        assert_eq!(h.prompt("NEXT", json!({})).await.unwrap()["text"], "OK");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn removed_root_in_captured_base_is_pruned_after_lua_setters_fail() {
    local(async {
        let env = TestEnv::new().await;
        let mut default_roots = vec![env.cwd.clone()];
        for path in [
            Some(std::path::PathBuf::from("/tmp")),
            Some(std::env::temp_dir()),
            std::env::var_os("TMPDIR").map(Into::into),
        ]
        .into_iter()
        .flatten()
        {
            if let Ok(path) = dunce::canonicalize(path) {
                default_roots.push(path);
            }
        }
        let mut candidates = vec![std::path::PathBuf::from("/var/tmp")];
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(home.into());
        }
        let parent = candidates
            .into_iter()
            .filter(|path| path.is_dir())
            .map(|path| dunce::canonicalize(path).unwrap())
            .find(|path| !default_roots.iter().any(|root| mare_binding::ossandbox::path_within(path, root)))
            .expect("a temporary parent outside the default workspace/temp roots");
        let outside = tempfile::Builder::new().prefix("mare-stale-extra-").tempdir_in(&parent).unwrap();
        let extra = dunce::canonicalize(outside.path()).unwrap();
        let extra_text = extra.to_string_lossy().to_string();
        let mut builder = env
            .host()
            .faux(json!([{ "text": "OK" }]))
            .args(json!({"trust":"trusted"}))
            .config(json!({"plugins":{"permissions":{"preset":"auto","enabled":true, "writable":[extra]}}}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        fake_ui(&h).await;
        let current_roots: Vec<String> = eval(&h, "local p=mare.proc.sandbox.current(); return p.writable").await;
        assert!(
            current_roots.iter().any(|root| root == &extra_text),
            "base preset must grant the configured extra: {current_roots:?}"
        );
        assert!(
            !current_roots
                .iter()
                .map(std::path::PathBuf::from)
                .filter(|root| root != &extra)
                .any(|root| mare_binding::ossandbox::path_within(&extra, &root)),
            "the extra must not be writable through a workspace/temp parent"
        );
        let session_path = env.write_session(
            "stale-default-root.jsonl",
            &[
                msg("m1", None, "user", "TARGET"),
                json!({"type":"custom", "kind":"sandbox/mode", "provenance":"session-host", "data":{"level":"read-only"}}),
            ],
        );
        global(&h, "TARGET", session_path.to_string_lossy().to_string());
        std::fs::remove_dir(&extra).unwrap();

        let (ok, err, switching, level): (bool, String, bool, String) = eval(
            &h,
            r#"
            local setter = mare.proc.sandbox.set_current
            mare.proc.sandbox.set_current = function(spec)
                if spec then return nil, 'injected setter failure before mutation' end
                return setter(spec)
            end
            local ok, err = pcall(S.load, S, { path = TARGET })
            mare.proc.sandbox.set_current = setter
            local p = mare.proc.sandbox.current()
            return ok, tostring(err), S:switching(), p and p.level or ''
        "#,
        )
        .await;
        assert!(ok && !switching, "{err}");
        assert_eq!(level, "workspace-write");
        assert!(notices(&h).await.contains("no longer exists"));
        std::fs::create_dir(&extra).unwrap();
        global(&h, "OUTSIDE", extra.to_string_lossy().to_string());
        let write_allowed: bool = eval(&h, "return mare.proc.sandbox.check('write', OUTSIDE .. '/new-file')").await;
        assert!(!write_allowed, "a recreated root omitted during fallback must stay outside the policy");
        assert_eq!(h.prompt("NEXT", json!({})).await.unwrap()["text"], "OK");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn sandbox_setter_failure_after_policy_mutation_keeps_the_poison_barrier() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "sandbox-setter-after-mutation.jsonl",
            &[
                msg("m1", None, "user", "TARGET"),
                json!({"type":"custom", "kind":"sandbox/mode", "provenance":"session-host", "data":{"level":"workspace-write"}}),
            ],
        );
        let h = env.host().faux(json!([{ "text": "OK" }])).build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, switching, level): (bool, String, bool, String) = eval(
            &h,
            r#"
            mare.proc.sandbox.set_current({level='read-only'})
            local setter = mare.proc.sandbox.set_current
            mare.proc.sandbox.set_current = function(spec, ...)
                local result = table.pack(setter(spec, ...))
                if spec and spec.level == 'workspace-write' then error('setter failed after policy mutation') end
                return table.unpack(result, 1, result.n)
            end
            local ok, err = pcall(S.load, S, { path = TARGET })
            mare.proc.sandbox.set_current = setter
            local current = mare.proc.sandbox.current()
            return ok, tostring(err), S:switching(), current and current.level or ''
        "#,
        )
        .await;
        assert!(!ok && switching && err.contains("setter failed after policy mutation"), "{err}");
        assert_eq!(level, "workspace-write", "the policy did mutate before the setter failed");
        assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn resume_model_listener_failure_poison_barrier() {
    local(async {
        for effort in [false, true] {
            let env = TestEnv::new().await;
            let record = if effort {
                json!({"type":"model", "effort":"high", "at_entry_id":"m1"})
            } else {
                json!({"type":"model", "model":"faux/echo2", "at_entry_id":"m1"})
            };
            let path = env.write_session("listener-failure.jsonl", &[msg("m1", None, "user", "TARGET"), record]);
            let h = env.host().faux(json!([{ "text":"OK" }])).build().await;
            expose(&h);
            global(&h, "TARGET", path.to_string_lossy().to_string());
            let (ok, err, switching): (bool, String, bool) = eval(
                &h,
                r#"
                C.cordis:on('llm/model-changed', function() error('listener failure') end)
                local ok, e = pcall(S.load, S, { path = TARGET })
                return ok, tostring(e), S:switching()
            "#,
            )
            .await;
            assert!(!ok && switching && err.contains("listener failure"), "effort={effort}: {err}");
            assert!(h.prompt("MMUST-NOT-RUN", json!({})).await.is_err());
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn session_add_workspace_dirs_and_set_sandbox_mode_rollback_on_persist_failure() {
    local(async {
        let env = TestEnv::new().await;
        let extra = dunce::canonicalize(&env.cwd).unwrap().parent().unwrap().join("rollback-extra");
        std::fs::create_dir_all(&extra).unwrap();

        let mut builder = env
            .host()
            .faux(json!([{ "text": "persisted" }]))
            .args(json!({"trust":"trusted"}))
            .config(json!({"plugins":{"permissions":{"preset":"auto","enabled":true}}}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        global(&h, "EXTRA", extra.to_string_lossy().to_string());

        exec(
            &h,
            r#"
            FAIL_PERSIST = false
            local orig_open = mare.store.open
            mare.store.open = function(path, opts)
                local f = orig_open(path, opts)
                if not opts or (not opts.auxiliary and not opts.create_new and not opts.existing) then
                    return setmetatable({}, {
                        __index = function(_, k)
                            if k == "append_batch" then
                                return function(_, entries, append_opts)
                                    if FAIL_PERSIST then
                                        return nil, "injected append_batch disk failure"
                                    end
                                    return f:append_batch(entries, append_opts)
                                end
                            end
                            local v = f[k]
                            if type(v) == "function" then
                                return function(_, ...)
                                    return v(f, ...)
                                end
                            end
                            return v
                        end,
                    })
                end
                return f
            end
        "#,
        )
        .await;

        let r = h.prompt("seed session file", json!({})).await.unwrap();
        assert_eq!(r["text"], "persisted", "{r}");

        let (
            ws_ok,
            sb_ok,
            dirs_unchanged,
            level_unchanged,
            roots_unchanged,
            gen_unchanged,
            entries_unchanged,
            ws_events,
            sb_events,
        ): (bool, bool, bool, bool, bool, bool, bool, i64, i64) = eval(
            &h,
            r#"
            FAIL_PERSIST = true
            local before_dirs = mare.json.encode(mare.workspace.dirs())
            local before_pol = mare.proc.sandbox.current()
            local before_level = before_pol and before_pol.level or ""
            local before_roots = mare.json.encode(before_pol and before_pol.roots or {})
            local before_gen = mare.proc.sandbox.generation()
            local before_entries = #S:entries()
            local ws_ev, sb_ev = 0, 0
            C.cordis:on("workspace/changed", function() ws_ev = ws_ev + 1 end)
            C.cordis:on("sandbox/changed", function() sb_ev = sb_ev + 1 end)

            local ws_ok = pcall(S.add_workspace_dirs, S, { EXTRA })
            local sb_ok = pcall(S.set_sandbox_mode, S, "read-only")

            local after_dirs = mare.json.encode(mare.workspace.dirs())
            local after_pol = mare.proc.sandbox.current()
            local after_level = after_pol and after_pol.level or ""
            local after_roots = mare.json.encode(after_pol and after_pol.roots or {})
            local after_gen = mare.proc.sandbox.generation()
            local after_entries = #S:entries()
            return ws_ok, sb_ok,
                before_dirs == after_dirs,
                before_level == after_level,
                before_roots == after_roots,
                before_gen == after_gen,
                before_entries == after_entries,
                ws_ev, sb_ev
        "#,
        )
        .await;
        assert!(!ws_ok, "add_workspace_dirs must error on persistence failure");
        assert!(!sb_ok, "set_sandbox_mode must error on persistence failure");
        assert!(dirs_unchanged, "workspace dirs must stay unchanged on persistence failure");
        assert!(level_unchanged, "sandbox level must stay unchanged on persistence failure");
        assert!(roots_unchanged, "sandbox roots must stay unchanged on persistence failure");
        assert!(gen_unchanged, "sandbox generation must stay unchanged on persistence failure");
        assert!(entries_unchanged, "S:entries() must stay unchanged on persistence failure");
        assert_eq!((ws_events, sb_events), (0, 0), "no change events may emit on persistence failure");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn session_add_workspace_dirs_persists_through_compaction_and_revalidates_on_resume() {
    use std::os::unix::fs::symlink;

    local(async {
        let env = TestEnv::new().await;
        let sibling_base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let parent = dunce::canonicalize(sibling_base.path()).unwrap();
        let keep = parent.join("compact-keep");
        let drop_deleted = parent.join("compact-drop-deleted");
        let drop_symlink = parent.join("compact-drop-symlink");
        let outside = parent.join("compact-outside");
        for d in [&keep, &drop_deleted, &drop_symlink, &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(keep.join("hello.txt"), "hi").unwrap();

        let mut builder = env
            .host()
            .faux(json!([{ "text": "seeded" }, { "text": "after-resume" }]))
            .args(json!({"trust":"trusted"}))
            .config(json!({"plugins":{"permissions":{"preset":"auto","enabled":true}}}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        global(&h, "KEEP", keep.to_string_lossy().to_string());
        global(&h, "DROP_DEL", drop_deleted.to_string_lossy().to_string());
        global(&h, "DROP_SYM", drop_symlink.to_string_lossy().to_string());

        assert_eq!(h.prompt("seed", json!({})).await.unwrap()["text"], "seeded");
        let (contains_keep, in_sandbox, path_str): (bool, bool, String) = eval(
            &h,
            r#"
            local res = S:add_workspace_dirs({ KEEP, DROP_DEL, DROP_SYM })
            assert(type(res) == "table" and type(res.dirs) == "table", "expected snapshot result")
            local contains = mare.workspace.contains(KEEP .. "/hello.txt")
            local pol = mare.proc.sandbox.current()
            local has_keep = false
            for _, r in ipairs(pol and pol.writable or {}) do
                if r == KEEP then has_keep = true end
            end
            local msgs = C.cordis:get("agent"):messages()
            S:append_compaction("compacted summary", msgs[#msgs], 10)
            return contains, has_keep, S:info().path
        "#,
        )
        .await;
        assert!(contains_keep, "mare.workspace must contain added directory");
        assert!(in_sandbox, "sandbox policy writable roots must contain added directory");
        h.shutdown().await;

        std::fs::remove_dir_all(&drop_deleted).unwrap();
        std::fs::remove_dir_all(&drop_symlink).unwrap();
        symlink(&outside, &drop_symlink).unwrap();

        let mut builder2 = env
            .host()
            .faux(json!([{ "text": "ok" }]))
            .args(json!({"trust":"trusted"}))
            .config(json!({"plugins":{"permissions":{"preset":"auto","enabled":true}}}));
        builder2.opts.project_trusted = true;
        let h2 = builder2.build().await;
        expose(&h2);
        fake_ui(&h2).await;
        global(&h2, "TARGET", path_str);
        global(&h2, "KEEP", keep.to_string_lossy().to_string());
        global(&h2, "DROP_DEL", drop_deleted.to_string_lossy().to_string());
        global(&h2, "DROP_SYM", drop_symlink.to_string_lossy().to_string());

        let (has_keep_ws, has_del_ws, has_sym_ws, has_keep_sb, has_del_sb, has_sym_sb): (
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
        ) = eval(
            &h2,
            r#"
            S:load({ path = TARGET })
            local function has(list, target)
                for _, item in ipairs(list or {}) do
                    if item == target then return true end
                end
                return false
            end
            local ws = mare.workspace.dirs()
            local sb = mare.proc.sandbox.current().writable or {}
            return has(ws, KEEP), has(ws, DROP_DEL), has(ws, DROP_SYM),
                   has(sb, KEEP), has(sb, DROP_DEL), has(sb, DROP_SYM)
        "#,
        )
        .await;
        assert!(has_keep_ws && has_keep_sb, "valid root must remain in workspace & sandbox after compaction+resume");
        assert!(!has_del_ws && !has_del_sb, "deleted root must be dropped from BOTH workspace & sandbox");
        assert!(!has_sym_ws && !has_sym_sb, "symlinked root must be dropped from BOTH workspace & sandbox");

        let note = notices(&h2).await;
        let del_lines = note.lines().filter(|l| l.contains(&drop_deleted.to_string_lossy().to_string())).count();
        let sym_lines = note.lines().filter(|l| l.contains(&drop_symlink.to_string_lossy().to_string())).count();
        assert_eq!(del_lines, 1, "expected exactly 1 warning line for deleted root: {note}");
        assert_eq!(sym_lines, 1, "expected exactly 1 warning line for symlinked root: {note}");
        h2.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn session_add_workspace_dirs_respects_ceiling_and_deny_read() {
    local(async {
        let env = TestEnv::new().await;
        let sibling_base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let parent = dunce::canonicalize(sibling_base.path()).unwrap();
        let denied_dir = parent.join("denied-read-dir");
        let outside_ceiling = parent.join("outside-ceiling-dir");
        let dot_git = env.cwd.join(".git");
        let dot_mare = env.cwd.join(".mare");
        for d in [&denied_dir, &outside_ceiling, &dot_git, &dot_mare] {
            std::fs::create_dir_all(d).unwrap();
        }

        let mut builder = env
            .host()
            .faux(json!([{ "text": "ok" }]))
            .args(json!({"trust":"trusted"}))
            .config(json!({"plugins":{"permissions":{"preset":"full-access","enabled":false}}}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        // Set ceiling to workspace-write (env.cwd only), and configure current policy with deny_read.
        let ceiling_policy = {
            let lua = h.lua();
            let spec = lua.create_table().unwrap();
            spec.set("level", "workspace-write").unwrap();
            spec.set("cwd", env.cwd.to_string_lossy().as_ref()).unwrap();
            spec.set("exclude_tmpdir_env_var", true).unwrap();
            spec.set("exclude_slash_tmp", true).unwrap();
            spec.set("deny_read", vec![denied_dir.to_string_lossy().to_string()]).unwrap();
            spec.set("accept_partial", vec!["fs_deny_read".to_string(), "fs_protect".to_string()]).unwrap();
            let val = mlua::Value::Table(spec);
            mare_binding::ossandbox::set_inherited_ceiling(
                lua,
                mare_binding::ossandbox::Policy::from_lua(lua, &val, &env.cwd).unwrap(),
            );
            true
        };
        assert!(ceiling_policy);

        global(&h, "DENIED_DIR", denied_dir.to_string_lossy().to_string());
        global(&h, "OUTSIDE_CEIL", outside_ceiling.to_string_lossy().to_string());
        global(&h, "DOT_GIT", dot_git.to_string_lossy().to_string());
        global(&h, "DOT_MARE", dot_mare.to_string_lossy().to_string());

        let (r_deny, r_ceil, r_git, r_mare, ws_len): (bool, bool, bool, bool, usize) = eval(
            &h,
            r#"
            local r1 = pcall(S.add_workspace_dirs, S, { DENIED_DIR })
            local r2 = pcall(S.add_workspace_dirs, S, { OUTSIDE_CEIL })
            local r3 = pcall(S.add_workspace_dirs, S, { DOT_GIT })
            local r4 = pcall(S.add_workspace_dirs, S, { DOT_MARE })
            return r1, r2, r3, r4, #mare.workspace.dirs()
        "#,
        )
        .await;
        assert!(!r_deny, "deny_read dir must be rejected");
        assert!(!r_ceil, "dir outside inherited sandbox ceiling must be rejected");
        assert!(!r_git, ".git dir must be rejected");
        assert!(!r_mare, ".mare dir must be rejected");
        assert_eq!(ws_len, 1, "rejected add_workspace_dirs calls must leave workspace untouched");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn session_add_workspace_dirs_allows_external_dir_when_cwd_has_deny_read() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".env"), "SECRET=1").unwrap();
        let sibling_base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let valid_extra = dunce::canonicalize(sibling_base.path()).unwrap().join("valid-extra");
        std::fs::create_dir_all(&valid_extra).unwrap();

        let mut builder = env
            .host()
            .faux(json!([{ "text": "ok" }]))
            .args(json!({"trust":"trusted"}))
            .config(json!({"plugins":{"permissions":{"preset":"auto","enabled":true}}}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        global(&h, "CWD_ENV", env.cwd.join(".env").to_string_lossy().to_string());
        global(&h, "VALID_EXTRA", valid_extra.to_string_lossy().to_string());
        global(&h, "CWD_PATH", env.cwd.to_string_lossy().to_string());

        let (ok, err, has_ws, has_sb): (bool, String, bool, bool) = eval(
            &h,
            r#"
            S:set_sandbox_mode({
                level = "workspace-write",
                cwd = CWD_PATH,
                deny_read = { CWD_ENV },
                accept_partial = { "fs_deny_read", "fs_protect" },
            })
            local ok, res = pcall(S.add_workspace_dirs, S, { VALID_EXTRA })
            local function has(list, target)
                for _, item in ipairs(list or {}) do
                    if item == target then return true end
                end
                return false
            end
            local sb = mare.proc.sandbox.current()
            return ok, ok and "" or tostring(res), has(mare.workspace.dirs(), VALID_EXTRA), has(sb and sb.writable or {}, VALID_EXTRA)
        "#,
        )
        .await;
        assert!(ok, "add_workspace_dirs must allow valid external dir when cwd contains deny_read: {err}");
        assert!(has_ws && has_sb, "valid external dir must be present in workspace dirs and sandbox writable roots");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn session_compaction_after_resume_drops_symlink_preserves_kept_sandbox_root() {
    use std::os::unix::fs::symlink;

    local(async {
        let env = TestEnv::new().await;
        let sibling_base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let parent = dunce::canonicalize(sibling_base.path()).unwrap();
        let keep = parent.join("keep");
        let swap_dir = parent.join("swap-dir");
        let outside = parent.join("outside");
        for d in [&keep, &swap_dir, &outside] {
            std::fs::create_dir_all(d).unwrap();
        }

        let mut builder = env
            .host()
            .faux(json!([{ "text": "seeded" }]))
            .args(json!({"trust":"trusted"}))
            .config(json!({"plugins":{"permissions":{"preset":"auto","enabled":true}}}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        global(&h, "KEEP", keep.to_string_lossy().to_string());
        global(&h, "SWAP_DIR", swap_dir.to_string_lossy().to_string());

        assert_eq!(h.prompt("seed", json!({})).await.unwrap()["text"], "seeded");
        let path_str: String = eval(
            &h,
            r#"
            S:add_workspace_dirs({ KEEP, SWAP_DIR })
            local msgs = C.cordis:get("agent"):messages()
            S:append_compaction("first compaction", msgs[#msgs], 10)
            return S:info().path
        "#,
        )
        .await;
        h.shutdown().await;

        std::fs::remove_dir_all(&swap_dir).unwrap();
        symlink(&outside, &swap_dir).unwrap();

        let resume_check = |target: String| async {
            let mut b = env
                .host()
                .faux(json!([{ "text": "ok" }]))
                .args(json!({"trust":"trusted"}))
                .config(json!({"plugins":{"permissions":{"preset":"auto","enabled":true}}}));
            b.opts.project_trusted = true;
            let host = b.build().await;
            expose(&host);
            fake_ui(&host).await;
            global(&host, "TARGET", target);
            global(&host, "KEEP", keep.to_string_lossy().to_string());
            global(&host, "SWAP_DIR", swap_dir.to_string_lossy().to_string());
            host
        };

        let h2 = resume_check(path_str.clone()).await;
        let (has_keep_ws, has_keep_sb, has_swap_ws, has_swap_sb): (bool, bool, bool, bool) = eval(
            &h2,
            r#"
            S:load({ path = TARGET })
            local msgs = C.cordis:get("agent"):messages()
            S:append_compaction("compacted after drop", msgs[#msgs], 10)
            local function has(list, target)
                for _, item in ipairs(list or {}) do
                    if item == target then return true end
                end
                return false
            end
            local ws = mare.workspace.dirs()
            local sb = mare.proc.sandbox.current().writable or {}
            return has(ws, KEEP), has(sb, KEEP), has(ws, SWAP_DIR), has(sb, SWAP_DIR)
        "#,
        )
        .await;
        assert!(has_keep_ws && has_keep_sb && !has_swap_ws && !has_swap_sb);
        h2.shutdown().await;

        let h3 = resume_check(path_str).await;
        let (has_keep_ws2, has_keep_sb2, has_swap_ws2, has_swap_sb2): (bool, bool, bool, bool) = eval(
            &h3,
            r#"
            S:load({ path = TARGET })
            local function has(list, target)
                for _, item in ipairs(list or {}) do
                    if item == target then return true end
                end
                return false
            end
            local ws = mare.workspace.dirs()
            local sb = mare.proc.sandbox.current().writable or {}
            return has(ws, KEEP), has(sb, KEEP), has(ws, SWAP_DIR), has(sb, SWAP_DIR)
        "#,
        )
        .await;
        assert!(
            has_keep_ws2 && has_keep_sb2,
            "keep must stay in workspace & sandbox writable roots after resume -> compact -> resume"
        );
        assert!(!has_swap_ws2 && !has_swap_sb2, "dropped symlink root must stay absent");
        h3.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn session_restore_validates_workspace_dirs_against_target_sandbox_not_previous() {
    local(async {
        let env = TestEnv::new().await;
        let sibling_base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let dir_x = dunce::canonicalize(sibling_base.path()).unwrap().join("dir-x");
        std::fs::create_dir_all(&dir_x).unwrap();

        let mut builder = env
            .host()
            .faux(json!([{ "text": "seeded" }]))
            .args(json!({"trust":"trusted"}))
            .config(json!({"plugins":{"permissions":{"preset":"auto","enabled":true}}}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "DIR_X", dir_x.to_string_lossy().to_string());
        global(&h, "CWD_PATH", env.cwd.to_string_lossy().to_string());

        assert_eq!(h.prompt("seed", json!({})).await.unwrap()["text"], "seeded");
        let (has_ws, has_sb): (bool, bool) = eval(
            &h,
            r#"
            S:add_workspace_dirs({ DIR_X })
            local session_b = S:info().path
            S:create()
            S:set_sandbox_mode({
                level = "workspace-write",
                cwd = CWD_PATH,
                deny_read = { DIR_X },
                accept_partial = { "fs_deny_read", "fs_protect" },
            })
            S:load({ path = session_b })
            local function has(list, target)
                for _, item in ipairs(list or {}) do
                    if item == target then return true end
                end
                return false
            end
            local sb = mare.proc.sandbox.current()
            return has(mare.workspace.dirs(), DIR_X), has(sb and sb.writable or {}, DIR_X)
        "#,
        )
        .await;
        assert!(
            has_ws && has_sb,
            "target session B's valid workspace dir must not be dropped by session A's stale deny_read"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn resume_cwd_missing_dir_offers_choose_another_directory_and_current_mode_keeps_launch_cwd() {
    local(async {
        let env = TestEnv::new().await;
        let gone_parent = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let gone_dir = dunce::canonicalize(gone_parent.path()).unwrap().join("gone-dir");
        std::fs::create_dir_all(&gone_dir).unwrap();
        let alt_dir = dunce::canonicalize(gone_parent.path()).unwrap().join("alt-dir");
        std::fs::create_dir_all(&alt_dir).unwrap();

        let path_missing = env.write_session("missing-cwd.jsonl", &[msg("m1", None, "user", "hello")]);
        let body = std::fs::read_to_string(&path_missing).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&gone_dir).unwrap(),
            1,
        );
        std::fs::write(&path_missing, &body).unwrap();
        std::fs::remove_dir(&gone_dir).unwrap();

        let h = env.host().faux(json!([{ "text": "ok" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path_missing.to_string_lossy().to_string());
        global(&h, "ALT", alt_dir.to_string_lossy().to_string());
        let (offered_choose, shell_cwd): (bool, String) = eval(
            &h,
            r#"
            local offered = false
            mare.ui.select = function(opts)
                for idx, item in ipairs(opts.items or {}) do
                    if item.label == "Choose another directory" then
                        offered = true
                        return idx
                    end
                end
                return 1
            end
            mare.ui.input = function() return ALT end
            S:load({ path = TARGET })
            return offered, C.cordis:get("tools"):cwd()
            "#,
        )
        .await;
        assert!(offered_choose, "missing session dir must offer 'Choose another directory'");
        assert_eq!(shell_cwd, alt_dir.to_string_lossy(), "choosing another directory must set shell_cwd");
        h.shutdown().await;

        // `resume_cwd = "current"` keeps launch directory without prompting even when recorded dir exists
        let path_existing = env.write_session("existing-other-cwd.jsonl", &[msg("m1", None, "user", "hi")]);
        let body2 = std::fs::read_to_string(&path_existing).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&alt_dir).unwrap(),
            1,
        );
        std::fs::write(&path_existing, &body2).unwrap();
        let h_cur = env
            .host()
            .config(json!({ "plugins": { "session": { "resume_cwd": "current" } } }))
            .faux(json!([{ "text": "ok" }]))
            .build()
            .await;
        expose(&h_cur);
        fake_ui(&h_cur).await;
        global(&h_cur, "TARGET2", path_existing.to_string_lossy().to_string());
        let (prompted, cur_cwd): (bool, String) = eval(
            &h_cur,
            r#"
            local prompted = false
            mare.ui.select = function() prompted = true return 1 end
            S:load({ path = TARGET2 })
            return prompted, C.cordis:get("tools"):cwd()
            "#,
        )
        .await;
        assert!(!prompted, "resume_cwd = 'current' must not prompt");
        assert_eq!(cur_cwd, env.cwd.to_string_lossy(), "resume_cwd = 'current' must stay in launch cwd");
        h_cur.shutdown().await;
    })
    .await
}
