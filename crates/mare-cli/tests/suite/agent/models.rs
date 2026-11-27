//! Model declarations, fallback routing, request hooks, faux provider validation, and media support.

use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn fallback_rebuilds_dynamic_tool_declarations_for_the_actual_model() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).config(json!({ "fallback_models": ["r7-middle/m", "r7-fallback/m"] })).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, r#"RAN, FALLBACKS, MIDDLE_TYPE, SEEN_TYPE, SEEN_FALLBACK_ONLY, SEEN_PRIMARY_ONLY = 0, 0, '', '', false, false
            L:register_provider({ provider = "r7-primary", is_configured = function() return true end,
                models = { { model_id = "m", context_window = 100000, max_output = 4096 } },
                stream = function() error({ message = "quota exhausted", kind = "quota", retryable = false }) end })
            L:register_provider({ provider = "r7-middle", is_configured = function() return true end,
                models = { { model_id = "m", context_window = 100000, max_output = 4096 } },
                stream = function(req)
                    for _, tool in ipairs(req.tools) do if tool.name == "dynamic_fallback" then MIDDLE_TYPE = tool.parameters.properties.v.type end end
                    error({ message = "quota exhausted", kind = "quota", retryable = false })
                end })
            L:register_provider({ provider = "r7-fallback", is_configured = function() return true end,
                models = { { model_id = "m", context_window = 100000, max_output = 4096,
                    params = { custom = { default = true, wire = { responses = function(req) req.test = true end } } } } },
                stream = function(req)
                    assert(type(req.model.params.custom.wire.responses) == "function")
                    FALLBACKS = FALLBACKS + 1
                    if FALLBACKS == 1 then
                        for _, tool in ipairs(req.tools) do
                            if tool.name == "dynamic_fallback" then SEEN_TYPE = tool.parameters.properties.v.type end
                            if tool.name == "fallback_only" then SEEN_FALLBACK_ONLY = true end
                            if tool.name == "primary_only" then SEEN_PRIMARY_ONLY = true end
                        end
                        return { role = "assistant", stop_reason = "tool_use", content = { { type = "tool_call", id = "fallback-call", name = "dynamic_fallback", arguments = { v = "hello" } } } }
                    end
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "done" } } }
                end })
            T:register("dynamic_fallback", { description = "probe", parameters = function(model)
                return { type = "object", properties = { v = { type = model.provider == "r7-primary" and "integer" or model.provider == "r7-middle" and "boolean" or "string" } }, required = { "v" } }
            end, execute = function(a) RAN = RAN + 1 return "ran:" .. a.v end })
            T:register("fallback_only", { description = "probe", enabled = function(m) return m.provider == "r7-fallback" end,
                parameters = { type = "object" }, execute = function() return "ok" end })
            T:register("primary_only", { description = "probe", enabled = function(m) return m.provider == "r7-primary" end,
                parameters = { type = "object" }, execute = function() return "ok" end })
            L:set_model("r7-primary/m", { save = false })"#).await;
        let result = h.prompt("go", json!({})).await.unwrap();
        let (middle, kind, fallback, primary, ran): (String, String, bool, bool, i64) = eval(&h, "return MIDDLE_TYPE, SEEN_TYPE, SEEN_FALLBACK_ONLY, SEEN_PRIMARY_ONLY, RAN").await;
        assert_eq!((middle.as_str(), kind.as_str(), fallback, primary, ran), ("boolean", "string", true, false, 1), "{result}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn dynamic_declarations_follow_same_ref_model_changes_between_turns() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, r#"TURNS, RAN = 0, 0
            SEEN, REQUEST_DEFS = {}, {}
            L:register_provider({ provider = "same-ref", is_configured = function() return true end,
                models = { { model_id = "m", context_window = 100000, max_output = 4096, choice = { kind = "integer" },
                    params = { custom = { default = true, wire = { responses = function() end } } } } },
                stream = function(req)
                    TURNS = TURNS + 1
                    assert(type(req.model.params.custom.wire.responses) == "function")
                    for _, tool in ipairs(req.tools) do
                        if tool.name == "same_ref_probe" then SEEN[TURNS] = tool.parameters.properties.v.type end
                    end
                    if TURNS == 1 then
                        return { role = "assistant", stop_reason = "tool_use", content = { { type = "tool_call", id = "change", name = "change_model", arguments = {} } } }
                    elseif TURNS == 2 then
                        return { role = "assistant", stop_reason = "tool_use", content = { { type = "tool_call", id = "probe", name = "same_ref_probe", arguments = { v = "hello" } } } }
                    end
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "done" } } }
                end })
            T:register("change_model", { description = "probe", parameters = { type = "object" }, execute = function()
                L:current().choice.kind = "string"
                return "changed"
            end })
            T:register("same_ref_probe", { description = "probe", parameters = function(model)
                return { type = "object", properties = { v = { type = model.choice.kind } }, required = { "v" } }
            end, execute = function(a) RAN = RAN + 1 return a.v end })
            C.cordis:on("request/build", function(req, next) REQUEST_DEFS[#REQUEST_DEFS + 1] = req.tools return next() end)
            L:set_model("same-ref/m", { save = false })"#).await;
        let result = h.prompt("go", json!({})).await.unwrap();
        let (first, second, third, rebuilt, reused, ran): (String, String, String, bool, bool, i64) =
            eval(&h, "return SEEN[1], SEEN[2], SEEN[3], REQUEST_DEFS[1] ~= REQUEST_DEFS[2], REQUEST_DEFS[2] == REQUEST_DEFS[3], RAN").await;
        assert_eq!((first.as_str(), second.as_str(), third.as_str(), rebuilt, reused, ran), ("integer", "string", "string", true, true, 1), "{result}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn fallback_request_hook_cannot_change_model_after_declarations() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).config(json!({ "fallback_models": ["guard-fallback/m"] })).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, r#"SENT = 0
            L:register_provider({ provider = "guard-primary", is_configured = function() return true end,
                models = { { model_id = "m", context_window = 100000, max_output = 4096 } },
                stream = function() error({ message = "quota exhausted", kind = "quota", retryable = false }) end })
            L:register_provider({ provider = "guard-fallback", is_configured = function() return true end,
                models = { { model_id = "m", context_window = 100000, max_output = 4096 } },
                stream = function() SENT = SENT + 1 return { role = "assistant", stop_reason = "stop", content = {} } end })
            C.cordis:on("request/build", function(req, next) if req.model.provider == "guard-fallback" then req.model.id = "changed" end return next() end)
            L:set_model("guard-primary/m", { save = false })"#).await;
        let result = h.prompt("go", json!({})).await.unwrap();
        assert_eq!(eval::<i64>(&h, "return SENT").await, 0, "{result}");
        assert_eq!(eval::<String>(&h, "return L:resolve('guard-fallback/m').id").await, "m", "fallback hook mutated the shared model");
        assert!(result.to_string().contains("changed the fallback model"), "{result}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn faux_provider_rejects_missing_and_non_object_tool_arguments() {
    local(async {
        for (label, arguments) in [
            ("missing", None), ("false", Some(json!(false))), ("null", Some(json!(null))),
            ("array", Some(json!([]))), ("string", Some(json!("{}"))),
        ] {
            let env = TestEnv::new().await;
            let mut call = json!({"name":"root_args_probe"});
            if let Some(value) = arguments { call["arguments"] = value; }
            let script = json!([{ "tool_calls": [call] }, { "text":"done" }]);
            let h = env.host().config(json!({ "model":"faux/echo", "plugins": { "provider-faux": { "script_json": script.to_string() } } })).no_session().build().await;
            expose(&h);
            exec(&h, r#"RAN, VOTES = 0, 0
                T:register("root_args_probe", { description = "probe", parameters = { type = "object" }, execute = function() RAN = RAN + 1 return "ran" end })
                C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "root_args_probe" then VOTES = VOTES + 1 end end)"#).await;
            let result = h.prompt("go", json!({})).await.unwrap();
            let (ran, votes): (i64, i64) = eval(&h, "return RAN, VOTES").await;
            assert_eq!((ran, votes), (0, 0), "{label}: {result}");
            assert!(result.to_string().contains("arguments"), "{label}: {result}");
            h.shutdown().await;
        }
    }).await
}

#[tokio::test]
async fn primary_request_hook_cannot_change_or_mutate_shared_model() {
    local(async {
        for replace in [false, true] {
            let env = TestEnv::new().await;
            let h = env.host().faux(json!([])).no_session().build().await;
            expose(&h);
            global(&h, "L", h.service_table("llm").unwrap());
            global(&h, "REPLACE", replace);
            exec(&h, r#"SENT = 0
                L:register_provider({ provider = "guard-main", is_configured = function() return true end,
                    models = { { model_id = "m", context_window = 100000, max_output = 4096 } },
                    stream = function() SENT = SENT + 1 return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "done" } } } end })
                C.cordis:on("request/build", function(req, next)
                    if REPLACE then req.model = { id = "changed", provider = "guard-main", ref = "guard-main/m", context_window = 100000, max_output = 4096 }
                    else req.model.id = "changed" end
                    return next()
                end)
                L:set_model("guard-main/m", { save = false })"#).await;
            let result = h.prompt("go", json!({})).await.unwrap();
            assert_eq!(eval::<i64>(&h, "return SENT").await, 0, "replace={replace}: {result}");
            assert_eq!(eval::<String>(&h, "return L:resolve('guard-main/m').id").await, "m", "replace={replace}");
            assert!(result.to_string().contains("changed the request model"), "{result}");
            h.shutdown().await;
        }
    }).await
}

#[tokio::test]
async fn primary_model_snapshot_preserves_function_valued_parameter_rules() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, r#"SENT = 0
            L:register_provider({ provider = "model-functions", is_configured = function() return true end,
                models = { { model_id = "m", context_window = 100000, max_output = 4096, extra_array = mare.json.array(),
                    params = { custom = { default = true, wire = { responses = function(req) req.test = true end } } } } },
                stream = function(req)
                    assert(type(req.model.params.custom.wire.responses) == "function")
                    assert(mare.json.encode(req.model.extra_array) == "[]")
                    SENT = SENT + 1 return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "done" } } }
                end })
            L:set_model("model-functions/m", { save = false })"#).await;
        let result = h.prompt("go", json!({})).await.unwrap();
        assert_eq!(eval::<i64>(&h, "return SENT").await, 1, "{result}");
        assert_eq!(result["text"], "done", "{result}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn faux_provider_rejects_script_table_configuration_with_diagnostic() {
    local(async {
        let env = TestEnv::new().await;
        let builder = env
            .host()
            .config(json!({"model":"faux/echo", "plugins":{"provider-faux":{"script":[{"text":"ok"}]}}}))
            .no_session();
        let h = mare_host::Host::new(builder.opts).await.unwrap();
        let failures: String = eval(&h, "return mare.json.encode(mare.plugins.failures())").await;
        assert!(failures.contains("provider-faux") && failures.contains("script_json"), "{failures}");
        let result = h.prompt("go", json!({})).await.unwrap();
        assert!(result["error"]["message"].is_string(), "{result}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn media_support_checks_all_fields_across_nil_holes_and_video_flag() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let (content_err, images_err, videos_err, audio_err, steer_err, follow_err, flagged_err): (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
        ) = eval(
            &h,
            r#"
            -- `attachments` is nil here; content/images/videos must still be inspected (no ipairs nil-hole skip).
            local ok_content, r_content = pcall(A.prompt, A, 'watch', {
                content = { { type = 'video', url = 'https://example.com/clip.mp4' } },
            })
            assert(not ok_content, 'content video must fail')
            local ok_images, r_images = pcall(A.prompt, A, 'watch', {
                images = { { kind = 'video', mime = 'video/mp4', data = 'AAAA' } },
            })
            assert(not ok_images, 'images video must fail')
            local ok_videos, r_videos = pcall(A.prompt, A, 'watch', {
                videos = { { path = 'clip.mp4' } },
            })
            assert(not ok_videos, 'videos field must fail')
            local ok_audio, r_audio = pcall(A.prompt, A, 'listen', {
                content = { { type = 'audio', mime = 'audio/pcm', data = 'AAAA' } },
            })
            assert(not ok_audio, 'content audio must fail')
            local ok_steer, r_steer = pcall(A.steer, A, 'watch', {
                content = { { type = 'video', url = 'https://example.com/clip.mp4' } },
            })
            assert(not ok_steer, 'steer with video must fail')
            local ok_follow, r_follow = pcall(A.prompt, A, 'watch', {
                images = { { mime = 'video/webm', data = 'AAAA' } },
                when_busy = 'queue',
            })
            assert(not ok_follow, 'follow_up with video must fail')

            -- Even when model.video == true, agent has no video transport and must reject explicitly.
            local llm = C.cordis:get('llm')
            local orig_current = llm.current
            llm.current = function(self)
                local m = orig_current(self) or {}
                m.video = true
                return m
            end
            local ok_flagged, r_flagged = pcall(A.prompt, A, 'watch', {
                content = { { type = 'video', url = 'https://example.com/clip.mp4' } },
            })
            assert(not ok_flagged, 'flagged video must fail')
            llm.current = orig_current

            return tostring(r_content),
                tostring(r_images),
                tostring(r_videos),
                tostring(r_audio),
                tostring(r_steer),
                tostring(r_follow),
                tostring(r_flagged)
            "#,
        )
        .await;
        assert!(content_err.contains("unsupported"), "content video must be rejected: {content_err}");
        assert!(images_err.contains("unsupported"), "images video must be rejected: {images_err}");
        assert!(
            videos_err.contains("unsupported") || videos_err.contains("invalid_arguments"),
            "videos field must be rejected: {videos_err}"
        );
        assert!(audio_err.contains("unsupported"), "content audio must be rejected: {audio_err}");
        assert!(steer_err.contains("unsupported"), "steer video must be rejected: {steer_err}");
        assert!(follow_err.contains("unsupported"), "follow_up video must be rejected: {follow_err}");
        assert!(
            flagged_err.contains("not implemented") || flagged_err.contains("unsupported"),
            "model.video=true without transport must still be rejected: {flagged_err}"
        );
        h.shutdown().await;
    })
    .await
}
