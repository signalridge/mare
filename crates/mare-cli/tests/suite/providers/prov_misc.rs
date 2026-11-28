//! PROV-MISC:
//! 1. fallback_worthy covers rate_limit/5xx after retries exhausted and plan-refusal 400/403 errors.
//! 2. Gemini converts non-whitelisted image MIMEs (e.g. gif, application/octet-stream) to PNG or degrades undecodable images to text notes.
//!
//! References:

use super::*;

#[tokio::test]
async fn fallback_triggers_on_exhausted_rate_limit_5xx_and_plan_refusal() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/compat",
            vec![
                // 1. rate_limit (429) retries=0 exhausted -> fallback
                Resp::status(429, r#"{"error":{"message":"rate limit exceeded"}}"#),
                // 2. server (500) retries=0 exhausted -> fallback
                Resp::status(500, r#"{"error":{"message":"internal server error"}}"#),
                // 3. plan refusal (403) -> fallback
                Resp::status(403, r#"{"error":{"message":"model is not available in the current token plan"}}"#),
                // 4. ordinary invalid (400) -> must NOT fallback
                Resp::status(400, r#"{"error":{"message":"invalid parameter temperature"}}"#),
            ],
        );
        server.route(
            "/anthropic",
            vec![
                Resp::sse(fixture("anthropic-text.sse")),
                Resp::sse(fixture("anthropic-text.sse")),
                Resp::sse(fixture("anthropic-text.sse")),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "fallback_models": ["anthropic/claude-sonnet-5-5"],
                "plugins": { "llm": { "retries": 0 } }
            }),
        )
        .await;

        let probe = r#"
local m = assert(LLM:resolve("compat/chat-model"))
return LLM:stream({
    model = m,
    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
}, function() end)
"#;

        for label in ["rate_limit", "5xx_server", "plan_refusal"] {
            let v: mlua::Value = h.lua().load(probe).eval_async().await.unwrap();
            let msg = mare_shared::lua::lua_to_json(h.lua(), v).unwrap();
            assert!(msg.get("error").is_none_or(Value::is_null), "{label}: expected fallback success, got {msg}");
            assert_eq!(msg["fallback_from"], json!("compat/chat-model"), "{label}: {msg}");
        }

        // Ordinary 400 invalid must NOT trigger fallback
        let v: mlua::Value = h.lua().load(probe).eval_async().await.unwrap();
        let msg = mare_shared::lua::lua_to_json(h.lua(), v).unwrap();
        assert_eq!(msg["stop_reason"], json!("error"), "{msg}");
        assert!(msg.get("fallback_from").is_none_or(Value::is_null), "{msg}");
        assert_eq!(server.count("/anthropic"), 3);

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn google_converts_non_whitelisted_image_mimes_and_degrades_undecodable_images() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/google", vec![Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        // 1x1 transparent GIF (base64)
        const GIF_1X1: &str = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
        // 1x1 PNG (base64)
        const PNG_1X1: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        // Undecodable SVG bytes (base64)
        const SVG_B64: &str = "PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciPjwvc3ZnPg==";

        let script = format!(
            r#"
local m = assert(LLM:resolve("google/gemini-2.5-flash"))
local msg = LLM:stream({{
    model = m,
    messages = {{
        {{
            role = "user",
            content = {{
                {{ type = "image", mime = "image/gif", data = "{GIF_1X1}" }},
                {{ type = "image", mime = "application/octet-stream", data = "{PNG_1X1}" }},
                {{ type = "image", mime = "image/svg+xml", data = "{SVG_B64}" }},
                {{ type = "image", mime = "image/jpeg; charset=binary", data = "{PNG_1X1}" }},
            }},
        }},
    }},
}}, function() end)
assert(msg and not msg.error, tostring(msg and msg.error))
"#
        );
        h.lua().load(&script).exec_async().await.unwrap();

        let body = server.requests().pop().unwrap().json();
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 4, "{parts:?}");
        // gif -> image/png
        assert_eq!(parts[0]["inlineData"]["mimeType"], json!("image/png"));
        assert_ne!(parts[0]["inlineData"]["data"], json!(GIF_1X1));
        // application/octet-stream -> image/png
        assert_eq!(parts[1]["inlineData"]["mimeType"], json!("image/png"));
        // undecodable svg -> text placeholder (never poisons history with invalid inlineData)
        assert!(parts[2].get("inlineData").is_none());
        assert!(parts[2]["text"].as_str().unwrap().contains("image/svg+xml"), "{}", parts[2]);
        // whitelisted mime with parameter normalized
        assert_eq!(parts[3]["inlineData"]["mimeType"], json!("image/jpeg"));

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn fallback_emits_reset_after_partial_stream_before_5xx() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        // Primary model streams partial text then emits an internal server error mid-stream (5xx).
        let partial_then_err = format!(
            "data: {}\n\ndata: {}\n\n",
            compat_text("HALF-PRIMARY-"),
            json!({"error":{"message":"upstream server error","type":"server_error","code":"internal_error"}}),
        );
        server.route("/compat", vec![Resp::sse(partial_then_err)]);
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "fallback_models": ["anthropic/claude-sonnet-5-5"],
                "plugins": { "llm": { "retries": 0 } }
            }),
        )
        .await;
        expose(&h);

        let v: mlua::Value = h
            .lua()
            .load(
                r#"
                local resets = {}
                C.cordis:on("llm/retry", function(ev)
                    resets[#resets + 1] = ev.reset == true
                end)
                local m = assert(LLM:resolve("compat/chat-model"))
                local msg = LLM:stream({
                    model = m,
                    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
                }, function() end)
                return { msg = msg, resets = resets }
                "#,
            )
            .eval_async()
            .await
            .unwrap();
        let out = mare_shared::lua::lua_to_json(h.lua(), v).unwrap();
        assert_eq!(out["msg"]["fallback_from"], json!("compat/chat-model"), "{out}");
        assert_eq!(out["resets"], json!([true]), "must emit llm/retry reset=true before fallback: {out}");
        assert_eq!(text_of(&out["msg"]), "All done.", "{out}");

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn fallback_handles_model_unserved_and_rejects_plain_401_403() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/compat",
            vec![
                // model_unserved -> must trigger fallback
                Resp::status(400, r#"{"error":{"message":"model_unserved for this region"}}"#),
                // plain 401 -> must NOT fallback
                Resp::status(401, r#"{"error":{"message":"invalid api key"}}"#),
                // plain 403 -> must NOT fallback
                Resp::status(403, r#"{"error":{"message":"permission denied"}}"#),
            ],
        );
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "fallback_models": ["anthropic/claude-sonnet-5-5"],
                "plugins": { "llm": { "retries": 0 } }
            }),
        )
        .await;

        let probe = r#"
local m = assert(LLM:resolve("compat/chat-model"))
return LLM:stream({
    model = m,
    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
}, function() end)
"#;

        let v1: mlua::Value = h.lua().load(probe).eval_async().await.unwrap();
        let m1 = mare_shared::lua::lua_to_json(h.lua(), v1).unwrap();
        assert_eq!(m1["fallback_from"], json!("compat/chat-model"), "{m1}");

        for status_label in ["plain_401", "plain_403"] {
            let v: mlua::Value = h.lua().load(probe).eval_async().await.unwrap();
            let msg = mare_shared::lua::lua_to_json(h.lua(), v).unwrap();
            assert_eq!(msg["stop_reason"], json!("error"), "{status_label}: {msg}");
            assert!(msg.get("fallback_from").is_none_or(Value::is_null), "{status_label}: {msg}");
        }
        assert_eq!(server.count("/anthropic"), 1);

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn host_prompt_emits_reset_and_replaces_partial_stream_on_fallback() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let partial_then_err = format!(
            "data: {}\n\ndata: {}\n\n",
            compat_text("PARTIAL-DISCARD-"),
            json!({"error":{"message":"upstream server error","type":"server_error","code":"internal_error"}}),
        );
        server.route("/compat", vec![Resp::sse(partial_then_err)]);
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "model": "compat/chat-model",
                "fallback_models": ["anthropic/claude-sonnet-5-5"],
                "plugins": { "llm": { "retries": 0 } }
            }),
        )
        .await;

        let res = h.prompt("hi", json!({})).await.unwrap();
        assert_eq!(res["text"], json!("All done."), "{res}");
        assert_eq!(server.count("/anthropic"), 1);

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn google_tool_result_images_normalize_jpg_nest_in_gemini3_and_degrade_oversized() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/google", vec![Resp::sse(fixture("google.sse")), Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        const GIF_1X1: &str = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
        const PNG_1X1: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        const BAD_IMG_B64: &str = "bm90LWEtcmVhbC1pbWFnZQ==";

        // 1. gemini-3-flash nests transcoded GIF + normalized image/jpg inside functionResponse.parts,
        // propagates non-PNG mime from mare.image.to_png (JPEG fallback), and degrades undecodable images.
        let script = format!(
            r#"
local orig_to_png = mare.image.to_png
local forced_jpeg_once = false
mare.image.to_png = function(data)
    if not forced_jpeg_once and data == "{GIF_1X1}" then
        -- First let the real binding transcode GIF -> PNG, wait for second call (synthetic JPEG fallback).
        return orig_to_png(data)
    elseif data == "synthetic-large-tiff" then
        forced_jpeg_once = true
        return {{ mime = "image/jpeg", data = "jpeg-fallback-data" }}
    end
    return orig_to_png(data)
end

local m = assert(LLM:resolve("google/gemini-3-pro-preview"))
local msg = LLM:stream({{
    model = m,
    messages = {{
        {{ role = "user", content = {{ {{ type = "text", text = "go" }} }} }},
        {{ role = "assistant", content = {{ {{ type = "tool_call", id = "c1", name = "snap", arguments = {{}} }} }} }},
        {{ role = "tool_result", tool_call_id = "c1", tool_name = "snap", content = {{
            {{ type = "image", mime = "image/gif", data = "{GIF_1X1}" }},
            {{ type = "image", mime = "image/jpg", data = "raw-jpg-bytes" }},
            {{ type = "image", mime = "image/tiff", data = "synthetic-large-tiff" }},
            {{ type = "image", mime = "image/bmp", data = "{BAD_IMG_B64}" }},
        }} }},
        {{ role = "user", content = {{
            {{ type = "image", mime = "image/png", data = "{PNG_1X1}" }},
        }} }},
    }},
}}, function() end)
mare.image.to_png = orig_to_png
assert(msg and not msg.error, tostring(msg and msg.error))
"#
        );
        h.lua().load(&script).exec_async().await.unwrap();

        let body = server.requests().pop().unwrap().json();
        let fn_resp = &body["contents"][2]["parts"][0]["functionResponse"];
        let nested = fn_resp["parts"].as_array().unwrap();
        assert_eq!(nested.len(), 3, "{fn_resp}");
        assert_eq!(nested[0]["inlineData"]["mimeType"], json!("image/png"));
        assert_eq!(nested[1]["inlineData"]["mimeType"], json!("image/jpeg"));
        assert_eq!(nested[1]["inlineData"]["data"], json!("raw-jpg-bytes"));
        assert_eq!(nested[2]["inlineData"]["mimeType"], json!("image/jpeg"));
        assert_eq!(nested[2]["inlineData"]["data"], json!("jpeg-fallback-data"));
        let fn_out = fn_resp["response"]["output"].as_str().unwrap();
        assert!(fn_out.contains("image/bmp"), "expected undecodable tool_result image note: {fn_out}");
        // Consecutive user turns merge in Gemini wire: user image sits at contents[2].parts[1].
        let merged_user_image = &body["contents"][2]["parts"][1];
        assert_eq!(merged_user_image["inlineData"]["mimeType"], json!("image/png"));

        h.shutdown().await;
    })
    .await;
}
