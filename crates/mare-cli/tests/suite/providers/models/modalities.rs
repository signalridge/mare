use super::*;

#[tokio::test]
async fn catalog_input_modalities_derive_image_capability_and_respect_overrides() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = server.base.clone();
        server.route(
            "/compat/models",
            vec![Resp::status(
                200,
                r#"{
                  "data": [
                    {
                      "id": "text-only",
                      "name": "Text Only",
                      "context_length": 32768,
                      "architecture": { "input_modalities": ["text"] }
                    },
                    {
                      "id": "vision-model",
                      "name": "Vision Model",
                      "context_length": 32768,
                      "architecture": { "input_modalities": ["text", "image"] }
                    },
                    {
                      "id": "unknown-model",
                      "name": "Unknown Modalities",
                      "context_length": 32768
                    },
                    {
                      "id": "overridden-text",
                      "name": "Overridden Text",
                      "context_length": 32768,
                      "architecture": { "input_modalities": ["text"] }
                    },
                    {
                      "id": "overridden-vision",
                      "name": "Overridden Vision",
                      "context_length": 32768,
                      "architecture": { "input_modalities": ["text", "image"] }
                    }
                  ]
                }"#,
            )],
        );
        server.route(
            "/compat/chat/completions",
            vec![Resp::sse(fixture("openai-compat.sse")), Resp::sse(fixture("openai-compat.sse"))],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "models": {
                    "localcompat/overridden-text": { "images": true },
                    "localcompat/overridden-vision": { "images": false }
                },
                "providers": {
                    "localcompat": {
                        "base_url": format!("{base}/compat"),
                        "no_key": true
                    }
                },
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 }
                }
            }),
        )
        .await;

        let (text_img, vision_img, unknown_desc_img, unknown_raw_img_nil, ov_text_img, ov_vision_img, raw_transformed_no_img): (
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
local out = LLM:refresh_catalog({ name = "localcompat", force = true })
assert(type(out.localcompat) == "number" and out.localcompat >= 3, "expected listed models, got: " .. tostring(out.localcompat))

local d_text = LLM:get_model("localcompat/text-only")
local d_vision = LLM:get_model("localcompat/vision-model")
local d_unknown = LLM:get_model("localcompat/unknown-model")
local r_unknown = assert(LLM:resolve("localcompat/unknown-model"))
local d_ov_text = LLM:get_model("localcompat/overridden-text")
local d_ov_vision = LLM:get_model("localcompat/overridden-vision")

local history = {
  { role = "user", content = { { type = "text", text = "look" }, { type = "image", data = "AAAA", mime = "image/png", path = "/tmp/shot.png" } } },
  { role = "assistant", provider = "other", content = {
      { type = "tool_call", id = "call_1", name = "read", arguments = { path = "/tmp/shot.png" } } } },
  { role = "tool_result", tool_call_id = "call_1", tool_name = "read", content = {
      { type = "image", data = "AAAA", mime = "image/png", path = "/tmp/tool.png" } } },
}
local converted = LLM:transform_messages(history, { id = "raw", provider = "localcompat", input = { "text" } })
local raw_has_image = false
for _, m in ipairs(converted) do
  for _, b in ipairs(m.content or {}) do
    if b.type == "image" then
      raw_has_image = true
    end
  end
end

local m_text = assert(LLM:resolve("localcompat/text-only"))
LLM:stream({ model = m_text, messages = history }, function() end)

local m_ov = assert(LLM:resolve("localcompat/overridden-text"))
LLM:stream({ model = m_ov, messages = history }, function() end)

return d_text.images, d_vision.images, d_unknown.images, r_unknown.images == nil, d_ov_text.images, d_ov_vision.images, not raw_has_image
"#,
        )
        .await;

        assert!(!text_img, "text-only catalog model must report images = false");
        assert!(vision_img, "vision catalog model must report images = true");
        assert!(unknown_desc_img, "unknown modalities model must default descriptor images = true");
        assert!(unknown_raw_img_nil, "unknown modalities internal model must keep images = nil");
        assert!(ov_text_img, "explicit images = true override must win over input_modalities=['text']");
        assert!(!ov_vision_img, "explicit images = false override must win over input_modalities=['text','image']");
        assert!(raw_transformed_no_img, "transform_messages must downgrade images for unnormalized input=['text'] model");

        let reqs: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|r| r.path == "/compat/chat/completions")
            .collect();
        assert_eq!(reqs.len(), 2, "expected 2 chat completion requests");

        let text_body = reqs[0].json();
        let text_str = text_body.to_string();
        assert!(
            !text_str.contains("image_url"),
            "text-only model must not receive image_url: {text_str}"
        );
        assert!(
            text_str.contains("Attachment: /tmp/shot.png") && text_str.contains("Attachment: /tmp/tool.png"),
            "text-only model must receive downgraded attachment text: {text_str}"
        );

        let ov_body = reqs[1].json();
        let ov_str = ov_body.to_string();
        assert!(
            ov_str.contains("image_url"),
            "overridden-text model with images=true must receive image_url: {ov_str}"
        );

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn catalog_listed_images_false_is_preserved_over_input_modalities() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;

        let (desc_images, raw_images_false, converted_has_no_image, converted_has_attachment): (
            bool,
            bool,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
LLM:register_provider({
  provider = "listed-images-test",
  is_configured = function() return true end,
  list_models = function()
    return {
      {
        id = "vision-disabled",
        name = "Vision Disabled",
        input = { "text", "image" },
        images = false,
      },
    }
  end,
})
local out = LLM:refresh_catalog({ name = "listed-images-test", force = true })
assert(out["listed-images-test"] == 1, "expected 1 listed model, got: " .. tostring(out["listed-images-test"]))

local d = LLM:get_model("listed-images-test/vision-disabled")
local r = assert(LLM:resolve("listed-images-test/vision-disabled"))

local history = {
  { role = "user", content = { { type = "text", text = "look" }, { type = "image", data = "AAAA", mime = "image/png", path = "/tmp/shot.png" } } },
  { role = "assistant", provider = "other", content = {
      { type = "tool_call", id = "call_1", name = "read", arguments = { path = "/tmp/shot.png" } } } },
  { role = "tool_result", tool_call_id = "call_1", tool_name = "read", content = {
      { type = "image", data = "AAAA", mime = "image/png", path = "/tmp/tool.png" } } },
}
local converted = LLM:transform_messages(history, r)
local has_image, has_attachment = false, false
for _, m in ipairs(converted) do
  for _, b in ipairs(m.content or {}) do
    if b.type == "image" then
      has_image = true
    elseif b.type == "text" and b.text == "Attachment: /tmp/shot.png" then
      has_attachment = true
    end
  end
end

return d.images, r.images == false, not has_image, has_attachment
"#,
        )
        .await;

        assert!(
            !desc_images,
            "listed model with input=['text','image'] and explicit images=false must report descriptor images=false"
        );
        assert!(
            raw_images_false,
            "listed model with explicit images=false must preserve raw m.images == false"
        );
        assert!(
            converted_has_no_image,
            "transform_messages must strip images when listed model has images=false"
        );
        assert!(
            converted_has_attachment,
            "transform_messages must downgrade image to Attachment text when listed model has images=false"
        );

        h.shutdown().await;
    })
    .await
}

/// Built-in seeds declare `input` without a redundant `images` flag so overriding only
/// `input = { "text" }` in user config cleanly derives `images = false`.
#[tokio::test]
async fn builtin_seed_input_override_to_text_only_disables_images() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "models": {
                    "deepseek/deepseek-flash": { "input": ["text"] },
                    "anthropic/claude-sonnet-5-5": { "input": ["text"] },
                    "openai/gpt-6-luna": { "input": ["text"] }
                }
            }),
        )
        .await;

        let (ds_img, ant_img, oai_img, pro_img): (bool, bool, bool, bool) = eval(
            &h,
            r#"
            return assert(LLM:resolve("deepseek/deepseek-flash")).images == true,
                assert(LLM:resolve("anthropic/claude-sonnet-5-5")).images == true,
                assert(LLM:resolve("openai/gpt-6-luna")).images == true,
                assert(LLM:resolve("deepseek/deepseek-v4-pro")).images == true
            "#,
        )
        .await;
        assert!(!ds_img, "deepseek-flash with input=['text'] override must derive images=false");
        assert!(!ant_img, "claude-sonnet-5-5 with input=['text'] override must derive images=false");
        assert!(!oai_img, "gpt-6-luna with input=['text'] override must derive images=false");
        assert!(!pro_img, "deepseek-v4-pro with input=['text'] seed must derive images=false");
        h.shutdown().await;
    })
    .await
}
