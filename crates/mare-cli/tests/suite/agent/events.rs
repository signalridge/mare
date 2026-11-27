//! Tool result states, lifecycle hook events, plugin subscriptions, and background jobs.

use crate::common::*;
use serde_json::json;

fn drain_cursor_no_loss(cursor: &mut mare_kernel::EventCursor) -> Vec<mare_kernel::TapEvent> {
    assert!(cursor.failure().borrow().is_none(), "{:?}", *cursor.failure().borrow());
    let mut out = Vec::new();
    while let Ok(Some(rec)) = cursor.try_recv() {
        match rec {
            mare_kernel::CursorRecord::Event { event, selected } if selected => out.push(event),
            mare_kernel::CursorRecord::Event { .. } => {}
            mare_kernel::CursorRecord::Dropped { count, .. } => panic!("unexpected dropped count {count}"),
            mare_kernel::CursorRecord::Lagged { from_seq, to_seq } => panic!("unexpected lagged {from_seq}..={to_seq}"),
        }
    }
    out
}

#[tokio::test]

async fn refused_tool_still_reports_a_model_visible_terminal_result() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "probe_refusal", "arguments": {} }] },
            { "text": "done" }
        ]);
        let h = env.host().builtins(&["jobs", "usage"]).faux(script).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"RAN = false
            T:register("probe_refusal", { description = "probe", parameters = { type = "object" },
              execute = function() RAN = true; return "ran" end })
            C.cordis:get("policy"):voter("tool/before", function(call)
              if call.name == "probe_refusal" then return {decision = "deny", reason = "test refusal"} end
            end)"#,
        )
        .await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let res = h.prompt("go", json!({})).await.unwrap();
        let events = drain_cursor_no_loss(&mut cursor);
        let item_ev =
            events.iter().find(|e| e.name == "item/completed" && e.payload["item"]["type"] == "tool_call").unwrap();
        let item = &item_ev.payload["item"];
        assert_eq!(item["result_status"], "denied");
        assert_eq!(item["execution_status"], "unknown");
        assert_eq!(item["tool_call_id"], item["message"]["tool_call_id"]);
        assert!(!events.iter().any(|e| e.name == "item/started"
            && e.payload["item"]["type"] == "tool_call"
            && e.payload["item"]["status"] == "running"));
        assert!(!eval::<bool>(&h, "return RAN").await);
        assert!(res["error"].is_null(), "{res}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn cancelled_tool_result_does_not_claim_execution_state() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([{ "tool_calls": [{ "name": "slow_result_probe", "arguments": {} }] }]);
        let h = env.host().builtins(&["jobs", "usage"]).faux(script).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"STARTED = false
            T:register("slow_result_probe", { description = "probe", parameters = { type = "object" },
              execute = function() STARTED = true; mare.task.sleep(60000); return "done" end })"#,
        )
        .await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let other = h.clone();
        let run = tokio::task::spawn_local(async move { other.prompt("go", json!({})).await.unwrap() });
        wait_until(60, "tool to start", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return STARTED").await }
        })
        .await;
        h.abort().await.unwrap();
        let res = tokio::time::timeout(std::time::Duration::from_secs(10), run).await.unwrap().unwrap();
        assert_eq!(res["error"]["code"], "aborted", "{res}");
        let events = drain_cursor_no_loss(&mut cursor);
        let item_ev =
            events.iter().find(|e| e.name == "item/completed" && e.payload["item"]["type"] == "tool_call").unwrap();
        let item = &item_ev.payload["item"];
        assert_eq!(item["result_status"], "cancelled");
        assert_eq!(item["execution_status"], "unknown");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn real_faux_tool_events_can_be_strictly_serialized() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("a.txt"), "alpha").unwrap();
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "a.txt" } }] },
            { "text": "done" }
        ]);
        let h = env.host().builtins(&["jobs", "usage"]).faux(script).no_session().build().await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|e| {
            (mare_protocol::event_registry::is_protocol_event(e) || e == "usage/changed").then_some(true)
        }));
        let res = h.prompt("go", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        let catalog = mare_protocol::schema();
        let contracts = h.kernel.list_events();
        let mut names = Vec::new();
        for ev in drain_cursor_no_loss(&mut cursor) {
            mare_protocol::events::validate_protocol(&ev.name, &ev.payload)
                .unwrap_or_else(|e| panic!("{}: {e}: {}", ev.name, ev.payload));
            let contract = contracts
                .iter()
                .find(|contract| contract.name == ev.name)
                .unwrap_or_else(|| panic!("no event contract for {}", ev.name));
            mare_protocol::validate_schema_at(&contract.data_schema, "#", &ev.payload)
                .unwrap_or_else(|e| panic!("{} violates its event contract: {e}: {}", ev.name, ev.payload));
            if ev.name == "turn/started" {
                let mut missing_id = ev.payload.clone();
                missing_id.as_object_mut().unwrap().remove("turn_id");
                assert!(mare_protocol::validate_schema_at(&contract.data_schema, "#", &missing_id).is_err());
            }
            if ev.name == "item/completed" && ev.payload["item"]["type"] == "tool_call" {
                crate::suite::tool_result_schema::assert_valid_tool_result_payload(&catalog, &ev.payload["item"]);
            }
            names.push(ev.name);
        }
        for name in [
            "turn/started",
            "step/started",
            "item/started",
            "item/delta",
            "item/completed",
            "step/completed",
            "turn/completed",
            "usage/changed",
        ] {
            assert!(names.iter().any(|n| n == name), "missing {name}: {names:?}");
        }
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn emitted_domain_event_payloads_match_their_contracts() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([{ "text": "done" }]))
            .config(json!({ "plugins": { "preset": "minimal", "permissions": { "enabled": true } } }))
            .build_exposed()
            .await;
        exec(
            &h,
            r#"
            CAPTURED_EVENTS = {}
            for _, name in ipairs({ "model/changed", "permission/changed", "session/changed", "agent/settled" }) do
                local event_name = name
                C.cordis:on(event_name, function(payload) CAPTURED_EVENTS[event_name] = payload end)
            end
            assert(C.cordis:get("llm"):set_model("faux/echo2"))
            assert(C.cordis:get("permissions"):set("read-only"))
            S:create()
        "#,
        )
        .await;
        let result = h.prompt("go", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let captured: String = eval(&h, "return mare.json.encode_strict(CAPTURED_EVENTS)").await;
        let captured: serde_json::Value = serde_json::from_str(&captured).unwrap();
        let contracts = h.kernel.list_events();
        for name in ["model/changed", "permission/changed", "session/changed", "agent/settled"] {
            let payload = captured.get(name).unwrap_or_else(|| panic!("{name} was not emitted: {captured}"));
            let contract = contracts.iter().find(|contract| contract.name == name).unwrap();
            mare_protocol::validate_schema_at(&contract.data_schema, "#", payload)
                .unwrap_or_else(|error| panic!("{name} violates its contract: {error}: {payload}"));
        }
        assert!(captured["model/changed"].get("model").is_none(), "{captured}");
        assert_eq!(captured["session/changed"]["reason"], "new");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn message_lifecycle_events_emit_in_order_and_listener_calls_are_timed() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("a.txt"), "alpha").unwrap();
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "a.txt" } }] },
            { "text": "hello world from faux stream" }
        ]);
        // every listener call is timed (slow_callback_ms = 0): per-call hook/start / hook/end are gone
        // on the cordis tree (CK-E A); a listener's timing is read from mare.profile
        let h = env.host().faux(script).config(json!({ "slow_callback_ms": 0 })).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            MSG_SEQ = {}
            C.cordis:on("item/started", function(ev)
                MSG_SEQ[#MSG_SEQ + 1] = { kind = "start", item_type = ev.item and ev.item.type }
            end)
            C.cordis:on("item/delta", function(ev)
                MSG_SEQ[#MSG_SEQ + 1] = { kind = "delta", delta_kind = ev.kind }
            end)
            C.cordis:on("item/completed", function(ev)
                MSG_SEQ[#MSG_SEQ + 1] = { kind = "completed", item_type = ev.item and ev.item.type }
            end)
            C.cordis:get("policy"):voter("tool/before", function() return "allow" end)
            "#,
        )
        .await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|e| {
            (mare_protocol::event_registry::is_protocol_event(e) || e == "usage/changed").then_some(true)
        }));
        let res = h.prompt("go", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");

        let tapped = drain_cursor_no_loss(&mut cursor);
        for ev in &tapped {
            mare_protocol::events::validate_protocol(&ev.name, &ev.payload)
                .unwrap_or_else(|e| panic!("{}: {e}: {}", ev.name, ev.payload));
        }

        // Verify item lifecycle ordering on tapped public stream:
        // item/started -> (item/delta for streaming assistant) -> item/completed, balanced per item_id.
        let item_events: Vec<_> = tapped
            .iter()
            .filter(|e| matches!(e.name.as_str(), "item/started" | "item/delta" | "item/completed"))
            .collect();
        let starts = item_events.iter().filter(|e| e.name == "item/started").count();
        let ends = item_events.iter().filter(|e| e.name == "item/completed").count();
        let deltas = item_events.iter().filter(|e| e.name == "item/delta").count();
        assert!(starts >= 4, "expected user + assistant(tool) + tool_call + assistant(text) starts: {item_events:?}");
        assert_eq!(starts, ends, "item/started and item/completed must balance: {item_events:?}");
        assert!(deltas >= 1, "expected at least one item/delta during stream: {item_events:?}");
        let mut active = std::collections::HashSet::new();
        for ev in &item_events {
            let item_id = ev.payload["item_id"].as_str().expect("item_id").to_string();
            match ev.name.as_str() {
                "item/started" => {
                    assert!(active.insert(item_id), "duplicate item/started: {item_events:?}");
                }
                "item/delta" => {
                    assert!(active.contains(&item_id), "item/delta outside active item lifecycle: {item_events:?}");
                }
                "item/completed" => {
                    assert!(active.remove(&item_id), "item/completed without active item/started: {item_events:?}");
                }
                _ => {}
            }
        }
        assert!(active.is_empty(), "unclosed items: {active:?}");

        // Lua listeners saw the same item lifecycle.
        let lua_msg_count: usize = eval(&h, "return #MSG_SEQ").await;
        assert!(lua_msg_count >= starts * 2 + deltas);

        // Listener calls are observable through mare.profile (CK-E A replaces per-call hook/*):
        // the policy listener (tool/before) and the item listeners were each timed.
        let timed: Vec<(String, i64, f64)> = eval::<Vec<mlua::Table>>(&h, "return mare.profile.callback_timings()")
            .await
            .into_iter()
            .map(|r| (r.get("event").unwrap(), r.get("count").unwrap(), r.get("max_ms").unwrap()))
            .collect();
        for event in ["tool/before", "item/started", "item/completed"] {
            assert!(
                timed.iter().any(|(e, n, ms)| e == event && *n >= 1 && *ms >= 0.0),
                "no timing for {event} listeners: {timed:?}"
            );
        }
        h.shutdown().await;
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn background_job_uses_private_handle_and_public_json_events() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs", "usage"]).no_session().build().await;
        expose(&h);
        global(&h, "JOBS", h.service_table("jobs").unwrap());
		exec(
			&h,
			"SAW_HANDLE = false; C.cordis:on('jobs/internal-changed', function(e) SAW_HANDLE = type(e.job.handle.wait) == 'function' end)",
		)
		.await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|event| {
            matches!(event, "jobs/changed" | "jobs/internal-changed").then_some(true)
        }));
        exec(&h, r#"JOBS:start({cmd = "true", kind = "test"})"#).await;
        let Some(mare_kernel::CursorRecord::Event { event, .. }) = cursor.recv().await else {
            panic!("expected jobs/changed event");
        };
        assert_eq!(event.name, "jobs/changed");
        assert_eq!(event.payload["job"]["kind"], "test");
        assert!(event.payload["job"].get("handle").is_none());
        assert!(cursor.try_recv().unwrap().is_none(), "process-local job event leaked to a frontend cursor");
		assert!(eval::<bool>(&h, "return SAW_HANDLE").await);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn non_json_public_plugin_extension_fails_its_subscription() {
    local(async {
        let env = TestEnv::new().await;
        let plugin_dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("myplugin.lua"),
            r#"return {
                name = "myplugin",
                contracts = {
                    myplugin = {
                        methods = {},
                        events = {
                            event = {
                                doc = "public plugin event",
                                mode = "emit",
                                expose = "both",
                                delivery = "required",
                                ignorable = false,
                                data = { type = "object", additionalProperties = true },
                                example = { kept = "value" },
                            },
                        },
                    },
                },
                apply = function() end,
            }"#,
        )
        .unwrap();
        let h = env.host().builtins(&["jobs", "usage"]).user_plugins().no_session().build().await;
        expose(&h);
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|name| (name == "myplugin/event").then_some(true)));
        exec(&h, r#"C.cordis:get("bus"):emit("myplugin/event", { extra = { answer = 42 }, kept = "value" })"#).await;
        let Some(mare_kernel::CursorRecord::Event { event: good, .. }) = cursor.recv().await else {
            panic!("expected good event")
        };
        assert_eq!(good.payload["extra"]["answer"], 42);
        assert_eq!(good.payload["kept"], "value");
        h.kernel
            .lua()
            .load(r#"C.cordis:get("bus"):emit("myplugin/event", { extra = function() end, kept = "value" })"#)
            .eval_async::<()>()
            .await
            .expect("unencodable public event must never throw to emitter");
        assert!(matches!(cursor.recv().await, Some(mare_kernel::CursorRecord::Dropped { count: 1, .. })));
        exec(&h, r#"C.cordis:get("bus"):emit("myplugin/event", { extra = { answer = 43 }, kept = "after" })"#).await;
        let Some(mare_kernel::CursorRecord::Event { event: after, .. }) = cursor.recv().await else {
            panic!("healthy cursor must keep receiving after dropped refusal")
        };
        assert_eq!(after.seq, good.seq + 1, "refused event must not allocate seq");
        assert!(cursor.failure().borrow().is_none());
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn model_changed_is_json_and_model_routes_stay_process_local() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs", "usage"]).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(
            &h,
            r#"SAW_MAPPING = false
			C.cordis:on("llm/model-changed", function(e)
              if e.model.provider == "map-probe" then
                SAW_MAPPING = type(e.model.params.custom.wire.responses) == "function"
              end
            end)
            L:register_provider({ provider = "map-probe", is_configured = function() return true end,
              models = { { model_id = "m", context_window = 100000, max_output = 4096,
                extra = "kept", params = { custom = { default = true,
                  wire = { responses = function() end, chat = { ["custom"] = "$value" } },
                  options = { { value = "one", wire = { responses = function() end, chat = { x = true } } } } } } } },
              stream = function() return {role = "assistant", stop_reason = "stop", content = {}} end })"#,
        )
        .await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|name| {
            matches!(name, "model/changed" | "llm/model-changed").then_some(true)
        }));
        exec(&h, r#"L:set_model("map-probe/m", {save = false})"#).await;
        let Some(mare_kernel::CursorRecord::Event { event, .. }) = cursor.recv().await else {
            panic!("expected model/changed event");
        };
        mare_protocol::events::validate_protocol(&event.name, &event.payload).unwrap();
        assert_eq!(event.payload["ref"], "map-probe/m");
        assert_eq!(event.payload["provider_id"], "map-probe");
        assert_eq!(event.payload["model_id"], "m");
        assert!(event.payload.get("model").is_none());
        assert!(cursor.try_recv().unwrap().is_none(), "process-local model event leaked to a frontend cursor");
        assert!(cursor.failure().borrow().is_none());
        assert!(eval::<bool>(&h, "return SAW_MAPPING").await);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn exception_fallback_has_an_empty_messages_array() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs", "usage"]).faux(json!([{ "text": "unused" }])).no_session().build().await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let result = h.prompt("go", json!({"messages": 42})).await.unwrap();
        assert!(result["error"]["message"].is_string(), "{result}");
        assert_eq!(result["messages"], json!([]), "{result}");
        let end =
            drain_cursor_no_loss(&mut cursor).into_iter().rfind(|ev| ev.name == "turn/completed").map(|ev| ev.payload);
        let end = end.expect("missing turn/completed");
        assert_eq!(end["status"], "failed", "{end}");
        mare_protocol::events::validate_protocol("turn/completed", &end).unwrap();
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn agent_start_projects_registered_secrets_inside_images() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "done" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            KEEP_IMG_DATA_SECRET = mare.projection.register("IMG_SECRET_DATA_99")
            KEEP_IMG_MIME_SECRET = mare.projection.register("IMG_SECRET_MIME_99")
            "#,
        )
        .await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|e| {
            (mare_protocol::event_registry::is_protocol_event(e) || e == "usage/changed").then_some(true)
        }));
        let res = h
            .prompt(
                "go",
                json!({
                    "images": [{
                        "type": "image",
                        "data": "prefix-IMG_SECRET_DATA_99-suffix",
                        "mime": "image/IMG_SECRET_MIME_99"
                    }]
                }),
            )
            .await
            .unwrap();
        assert!(res["error"].is_null(), "{res}");
        let start =
            drain_cursor_no_loss(&mut cursor).into_iter().find(|ev| ev.name == "turn/started").map(|ev| ev.payload);
        let start = start.expect("missing turn/started");
        mare_protocol::events::validate_protocol("turn/started", &start).unwrap();
        let serialized = start.to_string();
        assert!(!serialized.contains("IMG_SECRET_DATA_99"), "image data secret leaked: {serialized}");
        assert!(!serialized.contains("IMG_SECRET_MIME_99"), "image mime secret leaked: {serialized}");
        assert_eq!(start["images"].as_array().map(Vec::len), Some(1), "{start}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn agent_start_preserves_turn_id_when_secret_matches_field_name() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "done" }])).no_session().build().await;
        expose(&h);
        exec(&h, r#"KEEP_FIELD_NAME_SECRET = mare.projection.register("turn_id")"#).await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|e| {
            (mare_protocol::event_registry::is_protocol_event(e) || e == "usage/changed").then_some(true)
        }));
        let res = h.prompt("go", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        let events = drain_cursor_no_loss(&mut cursor);
        let start = events.iter().find(|ev| ev.name == "turn/started").map(|ev| ev.payload.clone());
        let end = events.iter().find(|ev| ev.name == "turn/completed").map(|ev| ev.payload.clone());
        let start = start.expect("missing turn/started");
        let end = end.expect("missing turn/completed");
        mare_protocol::events::validate_protocol("turn/started", &start).unwrap();
        mare_protocol::events::validate_protocol("turn/completed", &end).unwrap();
        assert!(res["turn_id"].as_str().is_some(), "missing turn_id in prompt result: {res}");
        assert_eq!(start["turn_id"], res["turn_id"], "turn/started turn_id must match prompt result: {start}");
        assert_eq!(end["turn_id"], res["turn_id"], "turn/completed turn_id must match prompt result: {end}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn agent_get_exposes_projected_registered_activity_and_provider_errors() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "done" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            KEEP_ACT_SECRET = mare.projection.register("ACTIVITY_SECRET_42")
            local ag = C.cordis:get("agent")
            ag:register_activity("plug.ok", function()
                return { status = "running", token = "prefix-ACTIVITY_SECRET_42-suffix", nested = { count = 3 } }
            end)
            ag:register_activity("plug.fail", function()
                error("activity boom")
            end)
            "#,
        )
        .await;
        let raw: String = eval(&h, r#"return mare.json.encode(C.cordis:get("agent"):get())"#).await;
        let snap: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let serialized = snap.to_string();
        assert!(!serialized.contains("ACTIVITY_SECRET_42"), "secret leaked in agent.get().activity: {serialized}");
        assert_eq!(snap["activity"]["extra"]["plug.ok"]["status"], "running", "{snap}");
        assert_eq!(snap["activity"]["extra"]["plug.ok"]["nested"]["count"], 3, "{snap}");
        let err_msg = snap["activity"]["extra_errors"]["plug.fail"].as_str().unwrap_or("");
        assert!(err_msg.contains("activity boom"), "expected provider error in extra_errors: {snap}");
        h.shutdown().await;
    })
    .await;
}

/// A `request/build` hook that mutates
/// `req.model` after tool declarations have been built is rejected on both primary and fallback
/// request paths.
#[tokio::test]
async fn request_build_hook_mutating_model_after_tool_declaration_is_rejected() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&[]).faux(json!([{ "text": "never" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"C.cordis:on("request/build", function(req, ev)
                 req.model = { ref = "faux/other", provider = "faux", id = "other" }
                 return ev.next(req)
               end)"#,
        )
        .await;
        let res = h.prompt("hi", json!({})).await.unwrap();
        let err = res["error"]["message"].as_str().unwrap_or("");
        assert!(
            err.contains("changed the request model after tool declaration"),
            "primary request/build model mutation must fail: {res}"
        );
        h.shutdown().await;

        let cfg = json!({
            "fallback_models": ["faux/echo"],
            "models": {
                "faux/primary": { "context_window": 100000, "max_output": 4096 }
            },
            "plugins": {
                "llm": { "retries": 0, "overloaded_retries": 0 }
            }
        });
        let h2 = env
            .host()
            .builtins(&[])
            .faux(json!([
                { "error": "upstream overloaded", "error_kind": "overloaded" },
                { "text": "fallback-never" }
            ]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h2);
        exec(
            &h2,
            r#"local llm = C.cordis:get("llm")
               llm:set_model("faux/primary", { save = false })
               local seen = 0
               C.cordis:on("request/build", function(req, ev)
                 seen = seen + 1
                 if seen >= 2 then
                   req.model = { ref = "faux/mutated", provider = "faux", id = "mutated" }
                 end
                 return ev.next(req)
               end)"#,
        )
        .await;
        let res2 = h2.prompt("hi", json!({})).await.unwrap();
        let err2 = res2["error"]["message"].as_str().unwrap_or("");
        assert!(
            err2.contains("changed the fallback model after tool declaration"),
            "fallback request/build model mutation must fail: {res2}"
        );
        h2.shutdown().await;
    })
    .await;
}
