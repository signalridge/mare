//! Tool refresh on list_changed and when a refresh may land relative to a turn.

use super::*;

/// tools/list_changed: three notifications in a burst are one refresh (300 ms debounce); new
/// tools are declared, withdrawn ones removed.
#[tokio::test]
async fn mcp_list_changed_refreshes_tools_once_per_burst() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "changed");
        let entry =
            fake.entry(json!({ "tools": ["echo", "change", "gone"], "changed_tools": ["echo", "change", "added"] }));
        let h = env
            .host()
            .builtins(&["mcp"])
            .config(servers(json!({ "fake": with(entry, json!({ "tier": "direct" })) })))
            .no_session()
            .build()
            .await;
        expose(&h);
        wait_until(15, "the tools", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__gone') ~= nil").await }
        })
        .await;
        exec(&h, r#"T:execute({ id = "c1", name = "mcp__fake__change", arguments = {} }, {})"#).await;
        wait_until(10, "the refreshed list", || {
            let h = h.clone();
            async move {
                eval::<bool>(&h, "return T:get('mcp__fake__added') ~= nil and T:get('mcp__fake__gone') == nil").await
            }
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(fake.count("tools/list"), 2, "initial + one debounced refresh: {:?}", fake.lines());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mcp_list_changed_updates_existing_tool_schema_and_safety() {
    local(async {
      for tier in ["direct", "deferred"] {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "changed-definition");
        let entry = fake.entry(json!({ "tools": ["echo", "change"], "changed_tools": ["echo", "change"],
            "change_definition": true }));
        let h = env
            .host().builtins(&["mcp"])
            .config(servers(json!({ "fake": with(entry, json!({ "tier": tier })) })))
            .no_session()
            .build()
            .await;
        expose(&h);
        if tier == "deferred" {
            exec(&h, r#"T:execute({ id = "search", name = "tool_search", arguments = { query = "select:echo,change" } }, {})"#).await;
        }
        wait_until(15, "the initial read-only tool", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:readonly('mcp__fake__echo')").await }
        })
        .await;
        exec(&h, r#"T:execute({ id = "c1", name = "mcp__fake__change", arguments = {} }, {})"#).await;
        wait_until(10, "the updated schema and safety classification", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"local t = T:get("mcp__fake__echo")
                       return t ~= nil and t.side_effect == "external" and t.parameters.required ~= nil
                         and t.parameters.required[1] == "text""#,
                )
                .await
            }
        })
        .await;
        h.shutdown().await;
      }
    })
    .await
}

#[tokio::test]
async fn mcp_refresh_detects_empty_array_versus_empty_object_in_schema() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "empty-default");
        let entry = fake.entry(json!({ "tools": ["echo", "change"], "changed_tools": ["echo", "change"],
            "change_empty_default": true }));
        let h = env
            .host()
            .builtins(&["mcp"])
            .config(servers(json!({ "fake": with(entry, json!({ "tier": "direct" })) })))
            .no_session()
            .build()
            .await;
        expose(&h);
        wait_until(15, "the initial tool", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__echo') ~= nil").await }
        })
        .await;
        let before: String =
            eval(&h, r#"return mare.json.encode(T:get("mcp__fake__echo").parameters.properties.text.default)"#).await;
        assert_eq!(before, "{}");
        exec(&h, r#"T:execute({ id = "c1", name = "mcp__fake__change", arguments = {} }, {})"#).await;
        wait_until(10, "the updated default", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"return mare.json.encode(T:get("mcp__fake__echo").parameters.properties.text.default) == "[]""#,
                )
                .await
            }
        })
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mcp_refresh_with_unchanged_definitions_reissues_revoked_generation() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "unchanged-definition");
        let entry = fake.entry(json!({ "tools": ["echo", "change"], "changed_tools": ["echo", "change"] }));
        let h = env
            .host()
            .builtins(&["mcp"])
            .config(servers(json!({ "fake": with(entry, json!({ "tier": "direct" })) })))
            .no_session()
            .build()
            .await;
        expose(&h);
        wait_until(15, "the initial tools", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__change') ~= nil").await }
        })
        .await;
        exec(
            &h,
            r#"BEFORE = T:revision() CHANGED = false
               C.cordis:on("mcp/changed", function() CHANGED = true end)
               T:execute({ id = "c1", name = "mcp__fake__change", arguments = {} }, {})"#,
        )
        .await;
        wait_until(10, "the refresh", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return CHANGED").await }
        })
        .await;
        let unchanged: bool = eval(&h, "return BEFORE == T:revision()").await;
        assert!(!unchanged, "the notification revokes prior declarations even when the refreshed schema is identical");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mcp_tool_refresh_waits_for_the_current_turn_to_finish() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "turn-boundary");
        let entry = fake.entry(json!({ "tools": ["echo", "change", "gone"],
            "changed_tools": ["echo", "change", "added"], "change_definition": true }));
        let h = env
            .host().builtins(&["mcp"])
            .config(servers(json!({ "fake": with(entry, json!({ "tier": "direct" })) })))
            .no_session()
            .build()
            .await;
        expose(&h);
        wait_until(15, "the initial tools", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__gone') ~= nil").await }
        })
        .await;
        exec(
            &h,
            r#"CHANGED = false
               C.cordis:on("mcp/changed", function() CHANGED = true end)
               C.cordis:get("bus"):emit("step/started", { turn_id = "t-1", step_id = "s-1", turn = 1 })
               T:execute({ id = "change", name = "mcp__fake__change", arguments = {} }, {})"#,
        )
        .await;
        wait_until(10, "the server refresh", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return CHANGED").await }
        })
        .await;
        let stable: bool = eval(
            &h,
            "return T:get('mcp__fake__gone') ~= nil and T:get('mcp__fake__added') == nil and T:readonly('mcp__fake__echo')",
        )
        .await;
        assert!(stable, "the in-flight turn still sees its original tool definitions");
        let updated: bool = eval(
            &h,
            r#"C.cordis:get("bus"):emit("step/completed", { turn_id = "t-1", step_id = "s-1", status = "completed" })
               return T:get("mcp__fake__gone") == nil and T:get("mcp__fake__added") ~= nil
                 and not T:readonly("mcp__fake__echo")"#,
        )
        .await;
        assert!(updated, "the refresh is applied at the turn boundary");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mcp_refresh_during_request_build_waits_for_the_real_turn_boundary() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "request-build");
        let entry = fake.entry(json!({ "tools": ["echo", "change", "gone"],
            "changed_tools": ["echo", "change", "added"], "change_definition": true }));
        let h = env
            .host()
            .builtins(&["mcp"])
            .faux(json!([{ "text": "done" }]))
            .config(servers(json!({ "fake": with(entry, json!({ "tier": "direct" })) })))
            .no_session()
            .build()
            .await;
        expose(&h);
        wait_until(15, "the initial tools", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__gone') ~= nil").await }
        })
        .await;
        exec(
            &h,
            r#"CHANGED = false
               C.cordis:on("mcp/changed", function() CHANGED = true end)
               C.cordis:on("request/build", function(req, next)
                 T:execute({ id = "change", name = "mcp__fake__change", arguments = {} }, {})
                 local deadline = mare.task.clock() + 5000
                 while not CHANGED and mare.task.clock() < deadline do mare.task.sleep(20) end
                 BUILD_STABLE = CHANGED and T:get("mcp__fake__gone") ~= nil
                   and T:get("mcp__fake__added") == nil and T:readonly("mcp__fake__echo")
                 return next()
               end)"#,
        )
        .await;
        let result = h.prompt("test", json!({})).await.unwrap();
        assert_eq!(result["text"], "done");
        let (stable, updated): (bool, bool) = eval(
            &h,
            r#"return BUILD_STABLE, T:get("mcp__fake__gone") == nil
                 and T:get("mcp__fake__added") ~= nil and not T:readonly("mcp__fake__echo")"#,
        )
        .await;
        assert!(stable, "the request-build hook must see the original definitions");
        assert!(updated, "the refresh must be applied after the turn");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mcp_late_startup_waits_until_interrupted_turn_ends() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "late-startup");
        // the server answers tools/list only once the test opens the gate: the listeners below are
        // registered and the turn has started before the late connection can finish (a fixed delay
        // raced the host build under load: mcp/ready is not sticky)
        let gate = env.home.path().join("late-startup.gate");
        let entry = fake.entry(json!({ "tools": ["echo"], "list_gate": gate }));
        let h = env
            .host().builtins(&["mcp"])
            .config(servers(json!({ "fake": with(entry, json!({ "tier": "direct" })) })))
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"READY, CHANGED = false, false
               C.cordis:on("mcp/ready", function() READY = true end)
               C.cordis:on("mcp/changed", function(ev) CHANGED = T:get('mcp__fake__echo') ~= nil and ev.server end)
               C.cordis:get("bus"):emit("step/started", { turn_id = "t-1", step_id = "s-1", turn = 1 })"#,
        )
        .await;
        std::fs::write(&gate, "").unwrap();
        wait_until(WAIT_SECS, "the delayed server", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return READY").await }
        })
        .await;
        let before: bool = eval(&h, "return T:get('mcp__fake__echo') == nil").await;
        assert!(before, "the late connection must not add tools during a request");
        let after: bool =
            eval(&h, r#"C.cordis:get("bus"):emit("turn/completed", { turn_id = "t-1", status = "completed" }) return T:get('mcp__fake__echo') ~= nil"#).await;
        assert!(after, "an interrupted request applies pending tools at turn/completed");
        // mcp/ready came with no tools (deferred): mcp/changed tells listeners once they exist
        let changed: String = eval(&h, "return tostring(CHANGED)").await;
        assert_eq!(changed, "fake", "mcp/changed after the deferred tools were registered");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mcp_invalid_schemas_are_isolated_and_refresh_withdraws_the_old_schema() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "invalid-schema");
        let entry = fake.entry(json!({ "tools": ["echo", "change", "broken", "huge"],
            "changed_tools": ["echo", "change", "broken", "huge"], "invalid_initial": true, "invalid_large": true, "invalid_after_change": true }));
        let h = env
            .host().builtins(&["mcp"])
            .config(servers(json!({ "fake": with(entry, json!({ "tier": "direct" })) })))
            .no_session()
            .build()
            .await;
        expose(&h);
        wait_until(15, "valid tools survive an invalid schema", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    "return T:get('mcp__fake__echo') ~= nil and T:get('mcp__fake__change') ~= nil",
                )
                .await
            }
        })
        .await;
        let bad_present: bool = eval(&h, "return T:get('mcp__fake__broken') ~= nil or T:get('mcp__fake__huge') ~= nil").await;
        assert!(!bad_present);
        exec(
            &h,
            r#"CHANGED = false C.cordis:on("mcp/changed", function() CHANGED = true end)
            T:execute({ id = "change", name = "mcp__fake__change", arguments = {} }, {})"#,
        )
        .await;
        wait_until(15, "invalid replacement withdraws the prior schema", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return CHANGED and T:get('mcp__fake__echo') == nil").await }
        })
        .await;
        let unaffected: bool = eval(&h, "return T:get('mcp__fake__change') ~= nil").await;
        assert!(unaffected);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn changed_mcp_tool_is_not_dispatched_after_refresh_during_model_stream() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "schema-during-stream");
        let entry = fake.entry(json!({ "tools": ["echo", "change"], "changed_tools": ["echo", "change"], "change_definition": true }));
        let script = json!([{ "text": "streaming ".repeat(80), "delay_ms": 30, "tool_calls": [{ "name": "mcp__fake__echo", "arguments": {} }] }, { "text": "done" }]);
        let h = env.host().builtins(&["mcp"]).faux(script).config(servers(json!({ "fake": with(entry, json!({ "tier": "direct" })) }))).no_session().build().await;
        expose(&h);
        wait_until(15, "the MCP tools", || { let h = h.clone(); async move { eval::<bool>(&h, "return T:get('mcp__fake__echo') ~= nil").await } }).await;
        exec(&h, r#"CHANGED, STREAMING, VOTES = false, false, 0
            C.cordis:on("item/delta", function(ev) if ev.kind == "text" then STREAMING = true end end)
            C.cordis:on("mcp/changed", function() CHANGED = true end)
            C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "mcp__fake__echo" then VOTES = VOTES + 1 end end)"#).await;
        let (result, ()) = tokio::join!(h.prompt("go", json!({})), async {
            wait_until(10, "provider stream", || { let h = h.clone(); async move { eval::<bool>(&h, "return STREAMING").await } }).await;
            exec(&h, r#"T:execute({ id = "change", name = "mcp__fake__change", arguments = {} }, {})"#).await;
            wait_until(10, "refresh observed while streaming", || { let h = h.clone(); async move { eval::<bool>(&h, "return CHANGED").await } }).await;
        });
        let result = result.unwrap();
        let votes: i64 = eval(&h, "return VOTES").await;
        assert_eq!(fake.count("tools/call"), 1, "only the change tool may reach the server: {result}");
        assert_eq!(votes, 0, "stale schema must be rejected before voting: {result}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn changed_mcp_tool_is_not_dispatched_after_approval_wait() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "schema-during-approval");
        let entry = fake.entry(
            json!({ "tools": ["echo", "change"], "changed_tools": ["echo", "change"], "change_definition": true }),
        );
        let script = json!([{ "tool_calls": [{ "name": "mcp__fake__echo", "arguments": {} }] }, { "text": "done" }]);
        let h = env
            .host().builtins(&["mcp"])
            .faux(script)
            .config(servers(json!({ "fake": with(entry, json!({ "tier": "direct" })) })))
            .no_session()
            .build()
            .await;
        expose(&h);
        wait_until(15, "the MCP tools", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__echo') ~= nil").await }
        })
        .await;
        exec(
            &h,
            r#"CHANGED, ASKED = false, false
            C.cordis:on("mcp/changed", function() CHANGED = true end)
            C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "mcp__fake__echo" then return "ask" end end)
            C.cordis:get("policy"):voter("tool/approval", function(call)
                if call.name == "mcp__fake__echo" then
                    ASKED = true
                    T:execute({ id = "change", name = "mcp__fake__change", arguments = {} }, {})
                    local deadline = mare.task.clock() + 5000
                    while not CHANGED and mare.task.clock() < deadline do mare.task.sleep(10) end
                    return { decision = "allow" }
                end
            end)"#,
        )
        .await;
        let result = h.prompt("go", json!({})).await.unwrap();
        let (asked, changed): (bool, bool) = eval(&h, "return ASKED, CHANGED").await;
        assert!(asked && changed, "approval did not wait for refresh: {result}");
        assert_eq!(fake.count("tools/call"), 1, "only the change tool may reach the server: {result}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cached_mcp_tool_rechecks_schema_when_first_connecting() {
    local(async {
        for changed in [false, true] {
            let env = TestEnv::new().await;
            let fake = Fake::new(&env, "cached-schema");
            let first = fake.entry(json!({ "tools": ["echo"] }));
            let h = env.host().builtins(&["mcp"]).config(servers(json!({ "fake": first }))).no_session().build().await;
            expose(&h);
            exec(
                &h,
                r#"T:execute({ id = "search", name = "tool_search", arguments = { query = "select:echo" } }, {})"#,
            )
            .await;
            h.shutdown().await;
            let second = fake.entry(json!({ "tools": ["echo"], "initial_schema_required": changed }));
            let h = env.host().builtins(&["mcp"]).config(servers(json!({ "fake": second }))).no_session().build().await;
            expose(&h);
            exec(
                &h,
                r#"T:execute({ id = "search", name = "tool_search", arguments = { query = "select:echo" } }, {})"#,
            )
            .await;
            assert_eq!(fake.count("start"), 1, "the second search must use the cache");
            let (failed, content): (bool, String) = eval(
                &h,
                r#"local r = T:execute({ id = "echo", name = "mcp__fake__echo", arguments = {} }, {})
                return r.is_error == true, type(r.content) == "table" and r.content[1].text or tostring(r.content)"#,
            )
            .await;
            if changed {
                assert!(failed, "stale cached tool ran after its schema changed: {content}");
                assert_eq!(fake.count("tools/call"), 0, "the stale call reached the server");
            } else {
                assert!(!failed && content.contains("echo"), "unchanged cached tool should run: {content}");
                assert_eq!(fake.count("tools/call"), 1);
            }
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn mcp_notification_invalidates_tool_before_debounced_refresh() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "notify-before-refresh");
        let entry = fake.entry(
            json!({ "tools": ["echo", "change"], "changed_tools": ["echo", "change"], "change_definition": true }),
        );
        let h = env
            .host()
            .builtins(&["mcp"])
            .config(servers(json!({ "fake": with(entry, json!({ "tier": "direct" })) })))
            .no_session()
            .build()
            .await;
        expose(&h);
        wait_until(15, "MCP tool", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__echo') ~= nil").await }
        })
        .await;
        let (before_refresh, refused): (bool, bool) = eval(
            &h,
            r#"local changed = false
            C.cordis:on("mcp/changed", function() changed = true end)
            C.cordis:get("bus"):emit("step/started", { turn_id = "t-1", step_id = "s-1", turn = 1 })
            T:execute({ id = "change", name = "mcp__fake__change", arguments = {} }, {})
            local d = T:get("mcp__fake__echo")
            local deadline = mare.task.clock() + 3000
            while d.is_current() and mare.task.clock() < deadline do mare.task.sleep(1) end
            local before = not changed and not d.is_current()
            local r = T:execute({ id = "echo", name = "mcp__fake__echo", arguments = {} }, {})
            return before, r.refused == true"#,
        )
        .await;
        assert!(before_refresh, "generation did not change until after debounced refresh");
        assert!(refused);
        assert_eq!(fake.count("tools/call"), 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn failed_mcp_refresh_keeps_old_tool_revoked() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "refresh-failure");
        let entry = fake.entry(json!({ "tools": ["echo", "change"], "changed_tools": ["echo", "change"], "fail_refresh": true }));
        let h = env.host().builtins(&["mcp"]).config(servers(json!({ "fake": with(entry, json!({ "tier": "direct" })) }))).no_session().build().await;
        expose(&h);
        wait_until(15, "MCP tool", || { let h = h.clone(); async move { eval::<bool>(&h, "return T:get('mcp__fake__echo') ~= nil").await } }).await;
        exec(&h, r#"C.cordis:get("bus"):emit("step/started", { turn_id = "t-1", step_id = "s-1", turn = 1 }) T:execute({ id = "change", name = "mcp__fake__change", arguments = {} }, {})"#).await;
        wait_until(10, "failed refresh", || { let fake = &fake; async move { fake.count("tools/list") >= 2 } }).await;
        let refused: bool = eval(&h, r#"local r = T:execute({ id = "echo", name = "mcp__fake__echo", arguments = {} }, {}) return r.refused == true"#).await;
        assert!(refused, "a failed refresh must not make the old schema usable");
        assert_eq!(fake.count("tools/call"), 1);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn changed_mcp_tool_during_vote_does_not_prompt_for_approval() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "schema-during-vote");
        let entry = fake.entry(
            json!({ "tools": ["echo", "change"], "changed_tools": ["echo", "change"], "change_definition": true }),
        );
        let h = env
            .host()
            .builtins(&["mcp"])
            .config(servers(json!({ "fake": with(entry, json!({ "tier": "direct" })) })))
            .no_session()
            .build()
            .await;
        expose(&h);
        wait_until(15, "MCP tool", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__echo') ~= nil").await }
        })
        .await;
        let (refused, asks): (bool, i64) = eval(
            &h,
            r#"ASKS = 0
            C.cordis:get("bus"):emit("step/started", { turn_id = "t-1", step_id = "s-1", turn = 1 })
            C.cordis:get("policy"):voter("tool/before", function(call)
                if call.name == "mcp__fake__echo" then
                    T:execute({ id = "change", name = "mcp__fake__change", arguments = {} }, {})
                    local d = T:get("mcp__fake__echo")
                    local deadline = mare.task.clock() + 3000
                    while d.is_current() and mare.task.clock() < deadline do mare.task.sleep(1) end
                    return "ask"
                end
            end)
            C.cordis:get("policy"):voter("tool/approval", function() ASKS = ASKS + 1 return { decision = "allow" } end)
            local r = T:execute({ id = "echo", name = "mcp__fake__echo", arguments = {} }, {})
            return r.refused == true, ASKS"#,
        )
        .await;
        assert!(refused);
        assert_eq!(asks, 0);
        assert_eq!(fake.count("tools/call"), 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mcp_disconnect_disposes_client_effect_without_leaking() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "effect-cleanup");
        let entry = fake.entry(json!({ "tools": ["echo"] }));
        let h = env
            .host()
            .builtins(&["mcp", "commands"])
            .config(servers(json!({ "fake": with(entry, json!({ "tier": "direct" })) })))
            .no_session()
            .build()
            .await;
        expose(&h);
        wait_until(15, "initial MCP connect", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"local s = C.cordis:get("mcp"):list_servers().data[1]
                       return s ~= nil and s.state == "connected" and T:get('mcp__fake__echo') ~= nil"#,
                )
                .await
            }
        })
        .await;
        let count_lua = r#"
            local n = 0
            local seen = {}
            local function add(f)
                if type(f) ~= "table" or seen[f] then return end
                seen[f] = true
                if f._disposables and type(f._disposables.values) == "function" then
                    for _, d in ipairs(f._disposables:values()) do
                        if type(d) == "table" then
                            for _, v in pairs(d) do
                                if type(v) == "table" and v.label == "cleanup" then
                                    n = n + 1
                                end
                            end
                        end
                    end
                end
            end
            add(C.cordis.fiber)
            local reg = C.cordis.registry
            if reg and type(reg.values) == "function" then
                for _, rt in ipairs(reg:values()) do
                    if rt.fibers and type(rt.fibers.values) == "function" then
                        for _, f in ipairs(rt.fibers:values()) do add(f) end
                    end
                end
            end
            return n
        "#;
        let base_connected: usize = eval(&h, count_lua).await;
        exec(&h, r#"C.cordis:get("commands"):run("/mcp disable fake")"#).await;
        wait_until(15, "MCP disabled", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"local s = C.cordis:get("mcp"):list_servers().data[1]
                       return s ~= nil and s.state == "disabled" and T:get('mcp__fake__echo') == nil"#,
                )
                .await
            }
        })
        .await;
        let base_disabled: usize = eval(&h, count_lua).await;
        assert_eq!(
            base_connected,
            base_disabled + 1,
            "connected MCP server must hold exactly 1 cleanup client_effect disposer (connected={base_connected}, disabled={base_disabled})"
        );
        for cycle in 1..=4 {
            exec(&h, r#"C.cordis:get("commands"):run("/mcp enable fake")"#).await;
            wait_until(15, "MCP reconnect", || {
                let h = h.clone();
                async move {
                    eval::<bool>(
                        &h,
                        r#"local s = C.cordis:get("mcp"):list_servers().data[1]
                           return s ~= nil and s.state == "connected" and T:get('mcp__fake__echo') ~= nil"#,
                    )
                    .await
                }
            })
            .await;
            let connected_now: usize = eval(&h, count_lua).await;
            assert_eq!(
                connected_now, base_connected,
                "cycle {cycle}: reconnecting MCP server must not accumulate client_effect disposers"
            );
            exec(&h, r#"C.cordis:get("commands"):run("/mcp disable fake")"#).await;
            wait_until(15, "MCP re-disabled", || {
                let h = h.clone();
                async move {
                    eval::<bool>(
                        &h,
                        r#"local s = C.cordis:get("mcp"):list_servers().data[1]
                           return s ~= nil and s.state == "disabled" and T:get('mcp__fake__echo') == nil"#,
                    )
                    .await
                }
            })
            .await;
            let disabled_now: usize = eval(&h, count_lua).await;
            assert_eq!(
                disabled_now, base_disabled,
                "cycle {cycle}: disconnecting MCP server must dispose client_effect cleanly"
            );
        }
        h.shutdown().await;
    })
    .await
}

/// Issue #187: valid standard JSON Schema MCP input schemas (using `$defs`/`$ref`, `anyOf`,
/// `minimum`, `pattern`, `format`) must register cleanly, enforce their constraints at call time,
/// and remain compatible with `tools:shadow` and `tools:bind_declarations`.
#[tokio::test]
async fn mcp_registers_tools_with_standard_json_schema_keywords() {
    local(async {
        let env = TestEnv::new().await;
        let script = env.home.path().join("std_schema_mcp.py");
        std::fs::write(
            &script,
            r##"import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    mid, meth = msg["id"], msg["method"]
    if meth == "initialize":
        res = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "std", "version": "1"}}
    elif meth == "tools/list":
        res = {"tools": [{
            "name": "query_items",
            "description": "Query items with standard JSON Schema",
            "inputSchema": {
                "type": "object",
                "$defs": {"tag": {"type": "string", "pattern": "^[a-z]+$"}},
                "properties": {
                    "limit": {"type": "integer", "minimum": 1},
                    "filter": {"anyOf": [{"$ref": "#/$defs/tag"}, {"type": "null"}]},
                    "email": {"type": "string", "format": "email"}
                },
                "required": ["limit"]
            }
        }]}
    elif meth == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        res = {"content": [{"type": "text", "text": f"queried:{args.get('limit')}:{args.get('filter')}"}]}
    else:
        res = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": res}) + "\n")
    sys.stdout.flush()
"##,
        )
        .unwrap();
        let entry = json!({
            "command": "python3",
            "args": [script.to_string_lossy()],
            "tier": "direct"
        });
        let h = env.host().builtins(&["mcp"]).config(servers(json!({ "std": entry }))).no_session().build().await;
        expose(&h);
        global(&h, "M", h.service_table("mcp").unwrap());
        wait_until(15, "std MCP server connected", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"for _, s in ipairs(M:list_servers().data) do
                         if s.name == "std" and s.state == "connected" then return true end
                       end
                       return false"#,
                )
                .await
            }
        })
        .await;

        let registered: bool = eval(&h, "return T:get('mcp__std__query_items') ~= nil").await;
        assert!(
            registered,
            "standard MCP inputSchema (with $defs/$ref, anyOf, minimum, pattern, format) was dropped at registration"
        );

        let (bad_min_err, bad_pat_err, ok_text, bind_ok, shadow_ok): (bool, bool, String, bool, bool) = eval(
            &h,
            r#"
            local r_min = T:execute({ id = "c1", name = "mcp__std__query_items", arguments = { limit = 0 } }, {})
            local r_pat = T:execute({ id = "c2", name = "mcp__std__query_items", arguments = { limit = 2, filter = "BAD123" } }, {})
            local r_ok = T:execute({ id = "c3", name = "mcp__std__query_items", arguments = { limit = 2, filter = "ok", email = "a@b.com" } }, {})
            local decls = T:definitions(nil, { "mcp__std__query_items" })
            local bound_ok = pcall(T.bind_declarations, T, decls)
            local def = T:get("mcp__std__query_items")
            def.description = "shadowed standard schema tool"
            local shadow_ok = pcall(T.shadow, T, "mcp__std__query_items", def)
            local ok_text = type(r_ok.content) == "table" and r_ok.content[1] and r_ok.content[1].text or r_ok.content
            return r_min.is_error == true, r_pat.is_error == true, tostring(ok_text), bound_ok, shadow_ok
            "#,
        )
        .await;
        assert!(bad_min_err, "minimum: 1 constraint must reject limit = 0");
        assert!(bad_pat_err, "$defs/$ref pattern constraint must reject invalid filter");
        assert_eq!(ok_text, "queried:2:ok");
        assert!(bind_ok, "bind_declarations must accept registered external MCP schema");
        assert!(shadow_ok, "tools:shadow must preserve external MCP schema");

        // Dynamic parameters(model) tools must share the exact same external-schema policy
        // (both via `external_schema = true` and via pre-compiled `mare.schema.compile_external`),
        // and `compile_value` handles must never bypass object-root tool schema requirements.
        let (dyn_flag_ok, dyn_flag_shadow_ok, dyn_handle_ok, scalar_rejected, obj_val_rejected): (
            bool,
            bool,
            bool,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
            T:register("dyn_ext_flag", {
                description = "dynamic external schema via flag",
                external_schema = true,
                parameters = function()
                    return { type = "object", properties = { limit = { type = "integer", minimum = 1 } }, required = { "limit" } }
                end,
                execute = function(args) return "ok:" .. tostring(args.limit) end,
            })
            local d1 = T:definitions(nil, { "dyn_ext_flag" })
            local b1 = pcall(T.bind_declarations, T, d1)
            local def1 = T:get("dyn_ext_flag")
            def1.description = "shadowed dynamic external"
            local s1 = pcall(T.shadow, T, "dyn_ext_flag", def1)
            local d1s = s1 and T:definitions(nil, { "dyn_ext_flag" }) or nil
            local b1s = s1 and pcall(T.bind_declarations, T, d1s) or false

            T:register("dyn_ext_handle", {
                description = "dynamic external schema via compiled handle",
                parameters = function()
                    return mare.schema.compile_external({
                        type = "object",
                        properties = { limit = { type = "integer", minimum = 2 } },
                        required = { "limit" },
                    })
                end,
                execute = function(args) return "ok:" .. tostring(args.limit) end,
            })
            local d2 = T:definitions(nil, { "dyn_ext_handle" })
            local b2 = pcall(T.bind_declarations, T, d2)

            local scalar_h = mare.schema.compile_value({ type = "string" })
            local obj_val_h = mare.schema.compile_value({ type = "object" })
            local reg_scalar_ok = pcall(T.register, T, "bad_scalar", {
                description = "must reject scalar compile_value",
                parameters = scalar_h,
                execute = function() return "no" end,
            })
            local reg_obj_val_ok = pcall(T.register, T, "bad_obj_val", {
                description = "must reject object compile_value",
                parameters = obj_val_h,
                execute = function() return "no" end,
            })
            return b1, b1s, b2, not reg_scalar_ok, not reg_obj_val_ok
            "#,
        )
        .await;
        assert!(dyn_flag_ok, "dynamic parameters(model) with external_schema=true must compile in definitions()");
        assert!(dyn_flag_shadow_ok, "shadowing dynamic external_schema tool must preserve external compilation");
        assert!(dyn_handle_ok, "dynamic parameters(model) returning mare.schema.compile_external must work");
        assert!(scalar_rejected, "tools:register must reject scalar compile_value handle");
        assert!(obj_val_rejected, "tools:register must reject non-object-root compile_value handle");
        h.shutdown().await;
    })
    .await
}
