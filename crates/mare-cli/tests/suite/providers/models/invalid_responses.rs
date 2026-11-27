use super::*;

#[tokio::test]
async fn codex_models_discovery_rejects_non_array_models_payload_and_preserves_cache() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/codex/codex/models",
            vec![
                Resp::status(
                    200,
                    r#"{"models":[{"slug":"gpt-6-cached","display_name":"GPT-6 Cached","visibility":"list"}]}"#,
                ),
                Resp::status(200, r#"{"models":{"error":"rate_limited"}}"#),
                Resp::status(200, r#"{"models":{}}"#),
                Resp::status(200, r#"{"models":[{"error":"rate_limited"}]}"#),
                Resp::status(
                    200,
                    r#"{"models":[null,{"slug":"gpt-6-new","display_name":"GPT-6 New","visibility":"list"}]}"#,
                ),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-openai": { "codex_base_url": format!("{}/codex", server.base) }
                }
            }),
        )
        .await;
        let (first_ok, err_msg, empty_obj_err, elem_err, null_elem_err, still_present): (
            bool,
            String,
            String,
            String,
            String,
            bool,
        ) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-1", refresh = "ref-1", expires = 9999999999999, account_id = "acct-1" }
}), { mode = 384 })
local out1 = LLM:refresh_catalog({ name = "openai", force = true })
local ok1 = type(out1.openai) == "number" and out1.openai >= 1 and LLM:resolve("openai/gpt-6-cached") ~= nil
local out2 = LLM:refresh_catalog({ name = "openai", force = true })
local out3 = LLM:refresh_catalog({ name = "openai", force = true })
local out4 = LLM:refresh_catalog({ name = "openai", force = true })
local out5 = LLM:refresh_catalog({ name = "openai", force = true })
local r = LLM:resolve("openai/gpt-6-cached")
return ok1, tostring(out2.openai), tostring(out3.openai), tostring(out4.openai), tostring(out5.openai), r ~= nil and r.listed == true
"#,
        )
        .await;
        assert!(first_ok, "initial discovery should succeed");
        assert!(err_msg.contains("invalid response"), "non-array models must fail: {err_msg}");
        assert!(empty_obj_err.contains("invalid response"), "empty object models must fail: {empty_obj_err}");
        assert!(elem_err.contains("invalid response"), "error-object element in models must fail: {elem_err}");
        assert!(null_elem_err.contains("invalid response"), "null element in models must fail: {null_elem_err}");
        assert!(still_present, "previously cached dynamic model must be preserved");
        let cache = std::fs::read_to_string(env.home.path().join("state/models-cache/openai.json")).unwrap();
        assert!(cache.contains("gpt-6-cached"), "disk cache must not be overwritten: {cache}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn codex_models_discovery_rejects_error_fields_and_preserves_cache() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/codex/codex/models",
            vec![
                Resp::status(
                    200,
                    r#"{"models":[{"slug":"gpt-6-cached","display_name":"GPT-6 Cached","visibility":"list"}]}"#,
                ),
                Resp::status(
                    200,
                    r#"{"error":{"message":"partial outage"},"models":[{"slug":"gpt-6-top-err","display_name":"GPT-6 Top Err","visibility":"list"}]}"#,
                ),
                Resp::status(
                    200,
                    r#"{"models":[{"slug":"gpt-6-elem-ok","display_name":"GPT-6 Elem Ok","visibility":"list"},{"slug":"gpt-6-elem-err","error":"model_unavailable"}]}"#,
                ),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-openai": { "codex_base_url": format!("{}/codex", server.base) }
                }
            }),
        )
        .await;
        let (first_ok, top_err, elem_err, still_present, top_absent, elem_ok_absent): (
            bool,
            String,
            String,
            bool,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-1", refresh = "ref-1", expires = 9999999999999, account_id = "acct-1" }
}), { mode = 384 })
local out1 = LLM:refresh_catalog({ name = "openai", force = true })
local ok1 = type(out1.openai) == "number" and out1.openai >= 1 and LLM:resolve("openai/gpt-6-cached") ~= nil
local out2 = LLM:refresh_catalog({ name = "openai", force = true })
local out3 = LLM:refresh_catalog({ name = "openai", force = true })
local r = LLM:resolve("openai/gpt-6-cached")
local r_top = LLM:resolve("openai/gpt-6-top-err")
local r_elem_ok = LLM:resolve("openai/gpt-6-elem-ok")
return ok1,
  tostring(out2.openai),
  tostring(out3.openai),
  r ~= nil and r.listed == true,
  r_top == nil or r_top.listed == nil,
  r_elem_ok == nil or r_elem_ok.listed == nil
"#,
        )
        .await;
        assert!(first_ok, "initial discovery should succeed");
        assert!(top_err.contains("invalid response"), "top-level error must fail: {top_err}");
        assert!(elem_err.contains("invalid response"), "element error must fail: {elem_err}");
        assert!(still_present, "previously cached dynamic model must be preserved");
        assert!(top_absent, "batch with top-level error must be rejected wholesale");
        assert!(elem_ok_absent, "batch with element error must be rejected wholesale");
        let cache = std::fs::read_to_string(env.home.path().join("state/models-cache/openai.json")).unwrap();
        assert!(cache.contains("gpt-6-cached"), "disk cache must not be overwritten: {cache}");
        assert!(!cache.contains("gpt-6-top-err"), "disk cache must not include rejected top-err batch: {cache}");
        assert!(!cache.contains("gpt-6-elem-ok"), "disk cache must not include rejected elem-err batch: {cache}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_api_key_models_discovery_rejects_error_and_non_array_data_and_preserves_cache() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/openai/models",
            vec![
                Resp::status(200, r#"{"object":"list","data":[{"id":"gpt-cached-api","name":"GPT Cached API"}]}"#),
                Resp::status(200, r#"{"error":{"message":"rate_limited"},"data":[{"id":"gpt-top-err"}]}"#),
                Resp::status(200, r#"{"data":{}}"#),
                Resp::status(200, r#"{"data":"bad"}"#),
                Resp::status(200, r#"{"data":[{"id":"gpt-elem-ok"},{"error":"model_unavailable"}]}"#),
                Resp::status(200, r#"{"data":[null,{"id":"gpt-after-null"}]}"#),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "catalog_refresh_ms": 0 } } })).await;
        let (
            first_ok,
            top_err,
            empty_obj_err,
            str_data_err,
            elem_err,
            null_elem_err,
            still_present,
            rejected_absent,
        ): (bool, String, String, String, String, String, bool, bool) = eval(
            &h,
            r#"
local out1 = LLM:refresh_catalog({ name = "openai", force = true })
local ok1 = type(out1.openai) == "number" and out1.openai >= 1 and LLM:resolve("openai/gpt-cached-api") ~= nil
local out2 = LLM:refresh_catalog({ name = "openai", force = true })
local out3 = LLM:refresh_catalog({ name = "openai", force = true })
local out4 = LLM:refresh_catalog({ name = "openai", force = true })
local out5 = LLM:refresh_catalog({ name = "openai", force = true })
local out6 = LLM:refresh_catalog({ name = "openai", force = true })
local r = LLM:resolve("openai/gpt-cached-api")
local r_top = LLM:resolve("openai/gpt-top-err")
local r_elem = LLM:resolve("openai/gpt-elem-ok")
local r_null = LLM:resolve("openai/gpt-after-null")
return ok1,
  tostring(out2.openai),
  tostring(out3.openai),
  tostring(out4.openai),
  tostring(out5.openai),
  tostring(out6.openai),
  r ~= nil and r.listed == true,
  (r_top == nil or r_top.listed == nil)
    and (r_elem == nil or r_elem.listed == nil)
    and (r_null == nil or r_null.listed == nil)
"#,
        )
        .await;
        assert!(first_ok, "initial discovery should succeed");
        assert!(top_err.contains("invalid response"), "top-level error must fail: {top_err}");
        assert!(empty_obj_err.contains("invalid response"), "empty object data must fail: {empty_obj_err}");
        assert!(str_data_err.contains("invalid response"), "non-array string data must fail: {str_data_err}");
        assert!(elem_err.contains("invalid response"), "element error must fail: {elem_err}");
        assert!(null_elem_err.contains("invalid response"), "null element must fail: {null_elem_err}");
        assert!(still_present, "previously cached dynamic model must be preserved");
        assert!(rejected_absent, "models from rejected batches must not be registered");
        let cache = std::fs::read_to_string(env.home.path().join("state/models-cache/openai.json")).unwrap();
        assert!(cache.contains("gpt-cached-api"), "disk cache must not be overwritten: {cache}");
        assert!(!cache.contains("gpt-top-err"), "disk cache must not include rejected top-err batch: {cache}");
        assert!(!cache.contains("gpt-elem-ok"), "disk cache must not include rejected elem-err batch: {cache}");
        assert!(!cache.contains("gpt-after-null"), "disk cache must not include rejected null-elem batch: {cache}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn anthropic_models_discovery_rejects_error_and_non_array_data_and_preserves_cache() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![
                Resp::status(
                    200,
                    r#"{"data":[{"id":"claude-cached-api","display_name":"Claude Cached API"}]}"#,
                ),
                Resp::status(
                    200,
                    r#"{"type":"error","error":{"type":"api_error","message":"overloaded"},"data":[{"id":"claude-top-err"}]}"#,
                ),
                Resp::status(200, r#"{"error":{"message":"rate_limited"}}"#),
                Resp::status(200, r#"{"data":{}}"#),
                Resp::status(200, r#"{"data":"bad"}"#),
                Resp::status(
                    200,
                    r#"{"data":[{"id":"claude-elem-ok"},{"error":"model_unavailable"}]}"#,
                ),
                Resp::status(200, r#"{"data":[null,{"id":"claude-after-null"}]}"#),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "catalog_refresh_ms": 0 } } })).await;
        let (
            first_ok,
            type_err,
            top_err,
            empty_obj_err,
            str_data_err,
            elem_err,
            null_elem_err,
            still_present,
            rejected_absent,
        ): (bool, String, String, String, String, String, String, bool, bool) = eval(
            &h,
            r#"
local out1 = LLM:refresh_catalog({ name = "anthropic", force = true })
local ok1 = type(out1.anthropic) == "number" and out1.anthropic >= 1 and LLM:resolve("anthropic/claude-cached-api") ~= nil
local out2 = LLM:refresh_catalog({ name = "anthropic", force = true })
local out3 = LLM:refresh_catalog({ name = "anthropic", force = true })
local out4 = LLM:refresh_catalog({ name = "anthropic", force = true })
local out5 = LLM:refresh_catalog({ name = "anthropic", force = true })
local out6 = LLM:refresh_catalog({ name = "anthropic", force = true })
local out7 = LLM:refresh_catalog({ name = "anthropic", force = true })
local r = LLM:resolve("anthropic/claude-cached-api")
local r_top = LLM:resolve("anthropic/claude-top-err")
local r_elem = LLM:resolve("anthropic/claude-elem-ok")
local r_null = LLM:resolve("anthropic/claude-after-null")
return ok1,
  tostring(out2.anthropic),
  tostring(out3.anthropic),
  tostring(out4.anthropic),
  tostring(out5.anthropic),
  tostring(out6.anthropic),
  tostring(out7.anthropic),
  r ~= nil and r.listed == true,
  (r_top == nil or r_top.listed == nil)
    and (r_elem == nil or r_elem.listed == nil)
    and (r_null == nil or r_null.listed == nil)
"#,
        )
        .await;
        assert!(first_ok, "initial discovery should succeed");
        assert!(type_err.contains("invalid response"), "type=error payload must fail: {type_err}");
        assert!(top_err.contains("invalid response"), "top-level error must fail: {top_err}");
        assert!(empty_obj_err.contains("invalid response"), "empty object data must fail: {empty_obj_err}");
        assert!(str_data_err.contains("invalid response"), "non-array string data must fail: {str_data_err}");
        assert!(elem_err.contains("invalid response"), "element error must fail: {elem_err}");
        assert!(null_elem_err.contains("invalid response"), "null element must fail: {null_elem_err}");
        assert!(still_present, "previously cached dynamic model must be preserved");
        assert!(rejected_absent, "models from rejected batches must not be registered");
        let cache = std::fs::read_to_string(env.home.path().join("state/models-cache/anthropic.json")).unwrap();
        assert!(cache.contains("claude-cached-api"), "disk cache must not be overwritten: {cache}");
        assert!(!cache.contains("claude-top-err"), "disk cache must not include rejected top-err batch: {cache}");
        assert!(!cache.contains("claude-elem-ok"), "disk cache must not include rejected elem-err batch: {cache}");
        assert!(!cache.contains("claude-after-null"), "disk cache must not include rejected null-elem batch: {cache}");
        h.shutdown().await;
    })
    .await
}
