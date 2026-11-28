//! Provider-dialect JSON schema sanitization for tool declarations and `output_schema`.
//! Reference tests:
//! - vercel/ai packages/google/src/sanitize-response-json-schema.test.ts:5-65
//! - vercel/ai packages/anthropic/src/sanitize-json-schema.test.ts:5-80
//! - vercel/ai packages/openai/src/normalize-openai-json-schema.test.ts:9-76

use super::*;

async fn lua_call(h: &Host, code: &str, args: Value) -> Value {
    let f = h.lua().load(code).into_function().unwrap();
    let mut mv = mlua::MultiValue::new();
    if let Value::Array(items) = args {
        for item in items {
            mv.push_back(mare_shared::lua::json_to_lua(h.lua(), &item).unwrap());
        }
    }
    let v: mlua::Value = f.call_async(mv).await.unwrap();
    mare_shared::lua::lua_to_json(h.lua(), v).unwrap()
}

#[tokio::test]
async fn schema_sanitize_unit_dialects_and_immutability() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let out = lua_call(
            &h,
            r#"
local S = require("mare_lib.schema_sanitize")

-- 1. DeepSeek \0 pattern escaping
assert(S.unicode_null_escape("^[^\\0]*$") == "^[^\\u0000]*$")
assert(S.unicode_null_escape("\\\\0") == "\\\\0")
assert(S.unicode_null_escape("\\01") == "\\01")
assert(S.unicode_null_escape("\\u0000") == "\\u0000")
assert(S.unicode_null_escape("\\\\\\0") == "\\\\\\u0000")

local ds_in = {
    type = "object",
    properties = {
        content = { type = "string", pattern = "^[^\\0]*$", default = { pattern = "\\0" } },
    },
    const = { pattern = "\\0" },
}
local ds_out = S.sanitize(ds_in, "deepseek", "https://api.deepseek.com/v1", "deepseek-chat")
assert(ds_out.properties.content.pattern == "^[^\\u0000]*$")
assert(ds_out.properties.content.default.pattern == "\\0", "default data must not be rewritten")
assert(ds_out.const.pattern == "\\0", "const data must not be rewritten")
assert(ds_in.properties.content.pattern == "^[^\\0]*$", "input schema must not be mutated")

-- 2. Kimi untyped enum / const + $defs + null union
local kimi_in = {
    type = "object",
    properties = {
        kinds = { anyOf = { { type = "array", items = { enum = { "text", "icon" } } }, { type = "null" } } },
        level = { enum = { 1, 2, 3 } },
        scale = { enum = { 1, 1.5 } },
        on = { const = true },
        mixed = { enum = { "a", 1 } },
        maybe = { enum = { "a", mare.json.null } },
        empty_arr_const = { const = mare.json.array() },
        empty_arr_enum = { enum = { mare.json.array() } },
        empty_obj_const = { const = {} },
        empty_obj_enum = { enum = { {} } },
    },
    ["$defs"] = {
        mode = { enum = { "fast", "slow" } },
    },
}
local kimi_out = S.sanitize(kimi_in, "kimi-for-coding", "https://api.kimi.com/coding/v1", "kimi-k2.5")
assert(kimi_out.properties.kinds.anyOf[1].items.type == "string")
assert(kimi_out.properties.level.type == "integer")
assert(kimi_out.properties.scale.type == "number")
assert(kimi_out.properties.on.type == "boolean")
assert(kimi_out.properties.mixed.type == nil, "mixed enum types must remain untyped")
assert(kimi_out.properties.maybe.type[1] == "string" and kimi_out.properties.maybe.type[2] == "null")
assert(kimi_out.properties.empty_arr_const.type == "array", "empty array const must infer array, not object")
assert(kimi_out.properties.empty_arr_enum.type == "array", "empty array enum must infer array, not object")
assert(kimi_out.properties.empty_obj_const.type == "object")
assert(kimi_out.properties.empty_obj_enum.type == "object")
assert(kimi_out["$defs"].mode.type == "string")
assert(kimi_in.properties.level.type == nil, "kimi input must not be mutated")

local ds_out_schema = S.sanitize(
    { type = "object", properties = { p = { type = "string", pattern = "^[^\\0]*$" } } },
    "deepseek",
    "https://api.deepseek.com/v1",
    "deepseek-reasoner",
    { output_schema = true }
)
assert(ds_out_schema.properties.p.pattern == "^[^\\u0000]*$")

-- 3. Grok Rust regex character-class escaping
assert(S.ecma_class_for_rust("^[^\\s[\\]]+$") == "^[^\\s\\[\\]]+$")
assert(S.ecma_class_for_rust("[[:alpha:]]") == "[\\[:alpha:]]")
assert(S.ecma_class_for_rust("[a&&b]") == "[a&\\&b]")
assert(S.ecma_class_for_rust("[+--]") == "[+-\\-]")
assert(S.ecma_class_for_rust("[~~]") == "[~\\~]")
assert(S.ecma_class_for_rust("a&&b--c~~d\\[x") == "a&&b--c~~d\\[x")
assert(S.ecma_class_for_rust("^[^][a-z]+$") == "^[\\s\\S][a-z]+$")
assert(S.ecma_class_for_rust("[]") == "[^\\s\\S]")

-- 4. OpenRouter/xgrammar oneOf -> anyOf without rejecting valid unprovably-disjoint schemas (lazy#46 #2)
local or_in = {
    type = "object",
    properties = {
        oneOf = { type = "string" },
        change = { oneOf = { { type = "object" }, { type = "null" } } },
        source = {
            oneOf = {
                {
                    type = "object",
                    properties = { path = { type = "string" } },
                    required = { "path" },
                    additionalProperties = false,
                },
                {
                    type = "object",
                    properties = { url = { type = "string" } },
                    required = { "url" },
                    additionalProperties = false,
                },
            },
        },
    },
}
local or_out = S.sanitize(or_in, "openrouter", "https://openrouter.ai/api/v1", "anthropic/claude-sonnet")
assert(or_out.properties.oneOf.type == "string", "property named oneOf must remain intact")
assert(or_out.properties.change.oneOf == nil and type(or_out.properties.change.anyOf) == "table")
assert(or_out.properties.source.oneOf == nil and #or_out.properties.source.anyOf == 2)
assert(or_in.properties.change.oneOf ~= nil, "openrouter input must not be mutated")

-- Combined oneOf + anyOf on same node does not error (kept intact when anyOf is already present)
local both_out = S.sanitize(
    { oneOf = { { type = "string" }, { type = "null" } }, anyOf = { { type = "boolean" } } },
    "openrouter",
    "https://openrouter.ai/api/v1",
    "anthropic/claude-sonnet"
)
assert(both_out.oneOf ~= nil and both_out.anyOf ~= nil)

-- Hostname boundary check (#7)
local not_or = S.detect_dialect("custom", "https://notopenrouter.ai.example.net/v1", "custom-model")
assert(not not_or.openrouter_xgrammar, "substring host must not match openrouter")
assert(not not_or.openai_strict)

-- 5. Google/Vertex missing type inference + const/enum intersection (codex json_schema_tests.rs:313)
local g_in = {
    type = "object",
    properties = {
        scope = { const = "global" },
        intersect = { const = "a", enum = { "a", "b" } },
        target = { anyOf = { { description = "box", properties = { x = { minimum = 0 } } }, { type = "null" } } },
        mixed_union = { anyOf = { { type = "string" }, { type = "number" } } },
        metadata = { properties = { extra = {} } },
    },
    ["$defs"] = {
        tag = { const = "v1" },
    },
}
local g_out = S.sanitize(g_in, "google", "https://generativelanguage.googleapis.com/v1beta", "gemini-2.5-flash")
assert(g_out.properties.scope.type == "string")
assert(g_out.properties.scope.const == nil and g_out.properties.scope.enum[1] == "global")
assert(g_out.properties.intersect.const == nil and #g_out.properties.intersect.enum == 1 and g_out.properties.intersect.enum[1] == "a")
assert(g_out["$defs"].tag.const == nil and g_out["$defs"].tag.enum[1] == "v1")
assert(g_out.properties.target.anyOf[1].type == "object")
assert(g_out.properties.target.anyOf[1].properties.x.type == "number")
assert(g_out.properties.target.type == nil, "nullable union must not drop null via parent type")
assert(g_out.properties.mixed_union.type == nil, "mixed union must remain untyped")
assert(g_out.properties.metadata.properties.extra.type == nil, "nested empty schema {} must stay open")
assert(g_in.properties.scope.const == "global", "google input must not be mutated")

local ok_empty_inter = pcall(
    S.sanitize,
    { const = "c", enum = { "a", "b" } },
    "google",
    "https://generativelanguage.googleapis.com/v1beta",
    "gemini-2.5-flash"
)
assert(not ok_empty_inter, "disjoint const+enum intersection must be rejected")

-- 6. Anthropic output_schema constraint stripping aligned with official docs (#8, #9)
local a_in = {
    type = "object",
    properties = {
        n = { type = { "number", "null" }, minimum = 1, maximum = 60 },
        s = { type = { "string", "null" }, description = "slug", minLength = 1, pattern = "^[a-z]+$", format = "idn-email" },
        look = { type = "string", pattern = "^(?=a)[a-z]+$", format = "email" },
    },
}
local a_out = S.sanitize(a_in, "anthropic", "https://api.anthropic.com", "claude-sonnet-5-5", { route = "anthropic", output_schema = true })
assert(a_out.additionalProperties == false)
assert(a_out.properties.n.minimum == nil and a_out.properties.n.maximum == nil)
assert(a_out.properties.n.description:find("minimum: 1", 1, true))
assert(a_out.properties.s.minLength == nil and a_out.properties.s.format == nil)
assert(a_out.properties.s.pattern == "^[a-z]+$", "simple pattern supported by Anthropic must be preserved")
assert(a_out.properties.s.description:find("slug", 1, true) and a_out.properties.s.description:find("format: idn-email", 1, true))
assert(a_out.properties.look.pattern == nil and a_out.properties.look.format == "email")
assert(a_out.properties.look.description:find("pattern: ^(?=a)[a-z]+$", 1, true))

-- 7. OpenAI strict output_schema propertyNames + additionalProperties (vercel openai normalize-openai-json-schema.test.ts:9-76)
local o_in = {
    type = "object",
    properties = {
        vars = { type = "object", propertyNames = { type = "string", pattern = "^[A-Z]+$" } },
    },
}
local o_out = S.sanitize(o_in, "openai", "https://api.openai.com/v1", "gpt-6.1-terra", { output_schema = true, strict = true })
assert(o_out.additionalProperties == false)
assert(o_out.properties.vars.additionalProperties == false)
assert(o_out.properties.vars.propertyNames == nil)
assert(o_in.properties.vars.propertyNames ~= nil, "openai input must not be mutated")

return { ok = true }
"#,
            json!([]),
        )
        .await;
        assert_eq!(out["ok"], json!(true));
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn schema_sanitize_recursive_children_inference_and_edge_cases() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let out = lua_call(
            &h,
            r#"
local S = require("mare_lib.schema_sanitize")

-- Non-table / non-string pass-throughs and dialect detection branches
assert(S.sanitize("raw", "openai") == "raw")
assert(S.unicode_null_escape(123) == 123)
assert(S.unicode_null_escape("plain") == "plain")
assert(S.ecma_class_for_rust(123) == 123)
assert(S.ecma_class_for_rust("[]a]") == "[^\\s\\S]a]")
assert(S.ecma_class_for_rust("[^]a]") == "[\\s\\S]a]")
assert(S.ecma_class_for_rust("[\\]a]") == "[\\]a]")

local d_sub = S.detect_dialect(nil, "https://sub.deepseek.com/v1", nil)
assert(d_sub.deepseek == true and d_sub.kimi == false)
local d_kimi = S.detect_dialect(nil, "https://api.moonshot.ai/v1", "moonshot-v1")
assert(d_kimi.kimi == true)
local d_grok = S.detect_dialect("xai", "https://api.x.ai/v1", "grok-3")
assert(d_grok.grok == true)

-- Recursive traversal across all schema-valued children + pre-detected dialect table
local deep_in = {
    type = "object",
    patternProperties = { ["^x_"] = { const = "a" } },
    definitions = { item = { enum = { 1, 2 } } },
    properties = {
        tup = { items = { { const = 1 }, { const = "s" } } },
        pref = { prefixItems = { { const = true } } },
        extra = { type = "object", additionalProperties = { const = "v" } },
        inter = { allOf = { { type = "string" }, { minLength = 2 } } },
        uni = { oneOf = { { type = "integer" }, { type = "integer" } } },
        arr = { minItems = 1 },
        str = { minLength = 2 },
        nil_only = { enum = { mare.json.null } },
        empty_enum = { enum = {} },
        bad_enum = { enum = { function() end } },
        nested_const = {
            const = { k = { 1, 2 } },
            enum = { { k = { 1, 2, 3 } }, { k = { 1, 2 } } },
        },
        cond = {
            ["if"] = { const = "a" },
            ["then"] = { const = "b" },
            ["else"] = { const = "c" },
            ["not"] = { const = "d" },
            propertyNames = { pattern = "^a$" },
            contains = { const = 1 },
        },
    },
}
local g_dialect = S.detect_dialect("google", "https://generativelanguage.googleapis.com/v1beta", "gemini-2.5-flash")
local deep_out = S.sanitize(deep_in, g_dialect)
assert(deep_out.patternProperties["^x_"].type == "string")
assert(deep_out.definitions.item.type == "integer")
assert(deep_out.properties.tup.items[1].type == "integer")
assert(deep_out.properties.tup.items[2].type == "string")
assert(deep_out.properties.pref.prefixItems[1].type == "boolean")
assert(deep_out.properties.extra.additionalProperties.type == "string")
assert(deep_out.properties.inter.type == "string")
assert(deep_out.properties.uni.type == "integer")
assert(deep_out.properties.arr.type == "array")
assert(deep_out.properties.str.type == "string")
assert(deep_out.properties.nil_only.type == "null")
assert(deep_out.properties.empty_enum.type == nil)
assert(deep_out.properties.bad_enum.type == nil)
assert(deep_out.properties.nested_const.const == nil and #deep_out.properties.nested_const.enum == 1)
assert(deep_out.properties.cond["if"].type == "string")
assert(deep_out.properties.cond["then"].type == "string")
assert(deep_out.properties.cond["else"].type == "string")
assert(deep_out.properties.cond["not"].type == "string")
assert(deep_out.properties.cond.contains.type == "integer")

-- Anthropic output_schema: multipleOf, exclusive bounds, maxLength, backreference pattern, supported format
local a_out = S.sanitize(
    {
        type = "object",
        properties = {
            num = { type = "integer", exclusiveMinimum = 0, exclusiveMaximum = 10, multipleOf = 2 },
            txt = { type = "string", maxLength = 32, pattern = "^(a)\\1$", format = "uuid" },
        },
    },
    "anthropic",
    "https://api.anthropic.com",
    "claude-sonnet-5-5",
    { output_schema = true }
)
assert(a_out.properties.num.exclusiveMinimum == nil and a_out.properties.num.multipleOf == nil)
assert(a_out.properties.num.description:find("multiple of: 2", 1, true))
assert(a_out.properties.txt.maxLength == nil and a_out.properties.txt.pattern == nil)
assert(a_out.properties.txt.format == "uuid")
assert(a_out.properties.txt.description:find("max length: 32", 1, true))

-- Anthropic tool input_schema: object-root oneOf flattening, conflicting property merge, shared required, and non-object rejection
local a_oneof_in = {
    type = "object",
    description = "Fetch resource",
    oneOf = {
        {
            properties = { common = { type = "string" }, target = { type = "string" }, path = { type = "string" } },
            required = { "common", "path" },
            additionalProperties = false,
        },
        {
            properties = { common = { type = "string" }, target = { type = "integer" }, url = { type = "string" } },
            required = { "common", "url" },
            additionalProperties = false,
        },
    },
}
local a_oneof_out = S.sanitize(a_oneof_in, "anthropic", "https://api.anthropic.com", "claude-sonnet-5-5", { route = "anthropic" })
assert(a_oneof_out.oneOf == nil, "top-level oneOf must be flattened for Anthropic tool schemas")
assert(a_oneof_out.type == "object")
assert(a_oneof_out.properties.path.type == "string" and a_oneof_out.properties.url.type == "string")
assert(a_oneof_out.properties.common.type == "string")
assert(type(a_oneof_out.properties.target.anyOf) == "table" and #a_oneof_out.properties.target.anyOf == 2)
assert(#a_oneof_out.required == 1 and a_oneof_out.required[1] == "common", "required must be branch intersection")
assert(a_oneof_out.additionalProperties == false)
assert(a_oneof_out.description:find("Fetch resource", 1, true) and a_oneof_out.description:find("required: common, path", 1, true))
assert(a_oneof_in.oneOf ~= nil, "input oneOf schema must not be mutated")

local ok_bad_root_oneof = pcall(
    S.sanitize,
    { type = "object", oneOf = { { type = "string" }, { type = "object", properties = { a = { type = "string" } } } } },
    "anthropic",
    "https://api.anthropic.com",
    "claude-sonnet-5-5",
    { route = "anthropic" }
)
assert(not ok_bad_root_oneof, "non-object branch in top-level oneOf must raise diagnostic error")

return { ok = true }
"#,
            json!([]),
        )
        .await;
        assert_eq!(out["ok"], json!(true));
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn wire_sanitizes_tool_and_output_schemas_per_provider() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        for (prefix, sse) in [
            ("/compat", "openai-compat.sse"),
            ("/google", "google.sse"),
            ("/anthropic", "anthropic.sse"),
            ("/openai", "openai-responses.sse"),
        ] {
            server.route(
                prefix,
                (0..5).map(|_| Resp::sse(fixture(sse))).collect(),
            );
        }
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "grok": {
                        "base_url": format!("{}/compat", server.base),
                        "dialect": "grok",
                        "api_key": "k",
                        "models": [{ "model_id": "grok-4.6", "name": "Grok" }]
                    },
                    "kimi": {
                        "base_url": format!("{}/compat", server.base),
                        "api_key": "k",
                        "models": [{ "model_id": "kimi-k2.5", "name": "Kimi" }]
                    },
                    "openrouter": {
                        "base_url": format!("{}/compat", server.base),
                        "api_key": "k",
                        "models": [{ "model_id": "or-m", "name": "OR" }]
                    },
                    "custom": {
                        "base_url": format!("{}/compat", server.base),
                        "api_key": "k",
                        "models": [{ "model_id": "m", "name": "Custom" }]
                    }
                }
            }),
        )
        .await;

        let lua_probe = r#"
local model_ref, tool_schema, out_schema = ...
local m = assert(LLM:resolve(model_ref), "unknown model " .. model_ref)
local req = {
    model = m,
    system = "sys",
    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
    tools = { { name = "probe_tool", description = "probe", parameters = tool_schema } },
    output_schema = out_schema,
}
local msg, err = LLM:stream(req, function() end)
assert(not err, tostring(err))
assert(msg and not msg.error, "stream error for " .. model_ref .. ": " .. tostring(msg and msg.error))
return {
    orig_tool_pattern = tool_schema.properties and tool_schema.properties.p and tool_schema.properties.p.pattern,
    orig_out_const = out_schema and out_schema.properties and out_schema.properties.k and out_schema.properties.k.const,
}
"#;

        // 1. DeepSeek: \0 -> \u0000 in both tool and output_schema
        let res = lua_call(
            &h,
            lua_probe,
            json!([
                "deepseek/deepseek-reasoner",
                { "type": "object", "properties": { "p": { "type": "string", "pattern": "^[^\\0]*$" } } },
                { "type": "object", "properties": { "p": { "type": "string", "pattern": "^[a-z]+$" } } }
            ]),
        )
        .await;
        assert_eq!(res["orig_tool_pattern"], json!("^[^\\0]*$"));
        let body: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(body["tools"][0]["function"]["parameters"]["properties"]["p"]["pattern"], json!("^[^\\u0000]*$"));
        assert_eq!(body["response_format"]["json_schema"]["schema"]["properties"]["p"]["pattern"], json!("^[a-z]+$"));

        // 2. Grok: Rust regex escaping inside character class
        lua_call(
            &h,
            lua_probe,
            json!([
                "grok/grok-4.6",
                { "type": "object", "properties": { "p": { "type": "string", "pattern": "^[^\\s[\\]]+$" } } },
                { "type": "object", "properties": { "p": { "type": "string", "pattern": "[a&&b]" } } }
            ]),
        )
        .await;
        let body: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(body["tools"][0]["function"]["parameters"]["properties"]["p"]["pattern"], json!("^[^\\s\\[\\]]+$"));
        assert_eq!(body["response_format"]["json_schema"]["schema"]["properties"]["p"]["pattern"], json!("[a&\\&b]"));

        // 3. Kimi: untyped enum gets inferred type
        lua_call(
            &h,
            lua_probe,
            json!([
                "kimi/kimi-k2.5",
                { "type": "object", "properties": { "mode": { "enum": ["a", "b"] } } },
                { "type": "object", "properties": { "lvl": { "enum": [1, 2] } } }
            ]),
        )
        .await;
        let body: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(body["tools"][0]["function"]["parameters"]["properties"]["mode"]["type"], json!("string"));
        assert_eq!(body["response_format"]["json_schema"]["schema"]["properties"]["lvl"]["type"], json!("integer"));

        // 4. OpenRouter: oneOf -> anyOf while property named 'oneOf' stays intact
        lua_call(
            &h,
            lua_probe,
            json!([
                "openrouter/or-m",
                { "type": "object", "properties": { "oneOf": { "type": "string" }, "v": { "oneOf": [{ "type": "string" }, { "type": "null" }] } } },
                { "type": "object", "properties": { "v": { "oneOf": [{ "type": "integer" }, { "type": "null" }] } } }
            ]),
        )
        .await;
        let body: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        let tp = &body["tools"][0]["function"]["parameters"]["properties"];
        assert_eq!(tp["oneOf"]["type"], json!("string"));
        assert!(tp["v"].get("oneOf").is_none() && tp["v"]["anyOf"].is_array());
        let os = &body["response_format"]["json_schema"]["schema"]["properties"]["v"];
        assert!(os.get("oneOf").is_none() && os["anyOf"].is_array());

        // 5. Google/Vertex: missing type inferred and const -> enum
        let res = lua_call(
            &h,
            lua_probe,
            json!([
                "google/gemini-2.5-flash",
                { "type": "object", "properties": { "k": { "const": "ok" }, "n": { "minimum": 1 } } },
                { "type": "object", "properties": { "k": { "const": "done" } } }
            ]),
        )
        .await;
        assert_eq!(res["orig_out_const"], json!("done"));
        let body: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        let gp = &body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["properties"];
        assert_eq!(gp["k"]["type"], json!("string"));
        assert_eq!(gp["k"]["enum"], json!(["ok"]));
        assert!(gp["k"].get("const").is_none());
        assert_eq!(gp["n"]["type"], json!("number"));
        let gos = &body["generationConfig"]["responseJsonSchema"]["properties"]["k"];
        assert_eq!(gos["type"], json!("string"));
        assert_eq!(gos["enum"], json!(["done"]));

        // 6. Anthropic: output_schema handles type arrays, preserves simple patterns, strips idn-* formats
        lua_call(
            &h,
            lua_probe,
            json!([
                "anthropic/claude-sonnet-5-5",
                { "type": "object", "properties": { "x": { "type": "string" } } },
                {
                    "type": "object",
                    "properties": {
                        "n": { "type": ["number", "null"], "minimum": 1 },
                        "s": { "type": ["string", "null"], "minLength": 2, "pattern": "^[a-z]+$", "format": "idn-email" }
                    }
                }
            ]),
        )
        .await;
        let body: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        let aos = &body["output_config"]["format"]["schema"];
        assert_eq!(aos["additionalProperties"], json!(false));
        assert!(aos["properties"]["n"].get("minimum").is_none());
        assert!(aos["properties"]["s"].get("minLength").is_none());
        assert!(aos["properties"]["s"].get("format").is_none());
        assert_eq!(aos["properties"]["s"]["pattern"], json!("^[a-z]+$"));

        // 7. OpenAI strict: output_schema strips propertyNames & sets additionalProperties: false
        lua_call(
            &h,
            lua_probe,
            json!([
                "openai/gpt-6.1-terra",
                { "type": "object", "properties": { "x": { "type": "string" } } },
                { "type": "object", "properties": { "m": { "type": "object", "propertyNames": { "type": "string" } } } }
            ]),
        )
        .await;
        let body: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        let oos = &body["text"]["format"]["schema"];
        assert_eq!(oos["additionalProperties"], json!(false));
        assert_eq!(oos["properties"]["m"]["additionalProperties"], json!(false));
        assert!(oos["properties"]["m"].get("propertyNames").is_none());

        // 8. Custom compat endpoint: does NOT apply OpenAI strict stripping or OpenRouter oneOf rewrite (#7)
        lua_call(
            &h,
            lua_probe,
            json!([
                "custom/m",
                { "type": "object", "properties": { "x": { "type": "string" } } },
                {
                    "type": "object",
                    "properties": {
                        "dict": { "type": "object", "propertyNames": { "type": "string" } },
                        "u": { "oneOf": [{ "type": "number" }, { "type": "integer" }] }
                    }
                }
            ]),
        )
        .await;
        let body: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        let cos = &body["response_format"]["json_schema"]["schema"];
        assert!(cos.get("additionalProperties").is_none(), "custom compat must not force additionalProperties:false");
        assert!(cos["properties"]["dict"]["propertyNames"].is_object(), "custom compat must preserve propertyNames");
        assert!(cos["properties"]["u"]["oneOf"].is_array(), "custom compat must preserve oneOf");

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn registered_tool_orders_nested_oneof_and_validates_runtime_args() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let tool_call = |id: &str, name: &str, args: &str| {
            json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": args }
                        }]
                    }
                }]
            })
        };
        let tool_stop = || json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
        server.route(
            "/compat",
            vec![
                Resp::sse(compat_events(&[
                    tool_call("c1", "ordered_union", r#"{"payload":{"kind":"a","zeta":"z","alpha":"a"}}"#),
                    tool_stop(),
                ])),
                Resp::sse(compat_events(&[
                    tool_call("c2", "ordered_union", r#"{"payload":{"kind":"a","zeta":"z","alpha":"a","bogus":true}}"#),
                    tool_stop(),
                ])),
                Resp::sse(compat_events(&[compat_text("done"), compat_stop()])),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "model": "openrouter/or-m",
                "providers": {
                    "openrouter": {
                        "base_url": format!("{}/compat", server.base),
                        "api_key": "k",
                        "models": [{ "model_id": "or-m", "name": "OR" }]
                    }
                }
            }),
        )
        .await;
        expose(&h);
        exec(
            &h,
            r#"
            _G.executed_calls = 0
            C.cordis:get("tools"):register("ordered_union", {
                description = "union tool with nested ordering",
                side_effect = "read",
                parameters = {
                    type = "object",
                    properties = {
                        payload = {
                            oneOf = {
                                {
                                    type = "object",
                                    ["x-mare-order"] = { "zeta", "alpha" },
                                    properties = {
                                        alpha = { type = "string" },
                                        kind = { const = "a" },
                                        zeta = { type = "string" },
                                    },
                                    required = { "kind", "zeta", "alpha" },
                                    additionalProperties = false,
                                },
                                {
                                    type = "object",
                                    properties = {
                                        kind = { const = "b" },
                                    },
                                    required = { "kind" },
                                    additionalProperties = false,
                                },
                            },
                        },
                    },
                    required = { "payload" },
                    additionalProperties = false,
                },
                execute = function()
                    _G.executed_calls = _G.executed_calls + 1
                    return "ok"
                end,
            })
            "#,
        )
        .await;
        let res = h.prompt("go", json!({})).await.unwrap();
        assert_eq!(res["text"], "done");
        let executed: i64 = eval(&h, "return _G.executed_calls").await;
        assert_eq!(executed, 1, "only valid args call should execute; bogus extra key must be rejected");
        let reqs = server.requests();
        let body: Value = serde_json::from_str(&reqs[0].body).unwrap();
        let tool = body["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "ordered_union").unwrap();
        let params = &tool["function"]["parameters"]["properties"]["payload"];
        assert!(params.get("oneOf").is_none() && params["anyOf"].is_array());
        assert!(!tool.to_string().contains("x-mare-order"), "nested x-mare-order must be stripped");
        let keys: Vec<&String> = params["anyOf"][0]["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys.first().map(|s| s.as_str()), Some("zeta"), "nested order in anyOf[0]: {keys:?}");
        let third_body: Value = serde_json::from_str(&reqs[2].body).unwrap();
        let msgs = third_body["messages"].as_array().unwrap();
        let rejected = msgs.iter().rfind(|m| m["role"] == "tool").unwrap();
        assert!(
            rejected["content"].as_str().unwrap_or("").contains("bogus")
                || rejected["content"].to_string().contains("Additional properties"),
            "{rejected}"
        );
        h.shutdown().await;
    })
    .await;
}

/// Unsupported JSON schema keywords (such as `$schema`, `allOf`, `anyOf`,
/// `patternProperties`, `minimum`) are rejected at tool registration with 0 HTTP requests, and
/// valid edge schemas serialize cleanly through all 4 built-in wires consistently with `guard_tool_declarations`.
#[tokio::test]
async fn unsupported_tool_schema_rejected_at_registration_and_edge_schemas_consistent_across_wires() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai/responses", vec![Resp::sse(responses_text("ok"))]);
        server.route("/compat/chat/completions", vec![Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]))]);
        server.route("/anthropic/v1/messages", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        server.route("/google", vec![Resp::sse(fixture("google.sse"))]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": { "deepseek": { "base_url": format!("{}/compat", server.base) } },
                "plugins": {
                    "llm": { "retries": 0 },
                    "provider-openai": { "base_url": format!("{}/openai", server.base), "transport": "http" },
                    "provider-anthropic": { "base_url": format!("{}/anthropic", server.base) },
                    "provider-google": { "base_url": format!("{}/google", server.base) }
                }
            }),
        )
        .await;
        expose(&h);

        let bad_rejected: bool = eval(
            &h,
            r#"
local bad = {
  { ["$schema"] = "https://json-schema.org/draft/2020-12/schema", type = "object", properties = {} },
  { type = "object", properties = { a = { allOf = { { type = "string" } } } } },
  { type = "object", properties = { a = { anyOf = { { type = "string" }, { type = "number" } } } } },
  { type = "object", patternProperties = { ["^x_"] = { type = "string" } } },
  { type = "object", properties = { n = { type = "integer", minimum = 1 } } },
}
local tools = C.cordis:get("tools")
for i, s in ipairs(bad) do
  local ok, err = pcall(tools.register, tools, "bad_schema_" .. i, {
    description = "bad",
    side_effect = "read",
    parameters = s,
    execute = function() return "no" end,
  })
  if ok then
    error("expected registration failure for schema #" .. i)
  end
  assert(tostring(err):find("unsupported", 1, true) ~= nil, "expected unsupported keyword error: " .. tostring(err))
end
return true
"#,
        )
        .await;
        assert!(bad_rejected);
        assert!(server.requests().is_empty(), "registration rejection must send 0 HTTP requests");

        let edge_ok: bool = eval(
            &h,
            r#"
local edge_schema = {
  type = "object",
  ["x-mare-order"] = { "mode", "tags", "meta" },
  properties = {
    mode = {
      oneOf = {
        { type = "object", properties = { kind = { const = "a" } }, required = { "kind" }, additionalProperties = false },
        { type = "object", properties = { kind = { const = "b" }, label = { type = { "string", "null" } } }, required = { "kind" }, additionalProperties = false },
      },
    },
    tags = { type = "array", items = { type = "string", enum = { "fast", "safe" } } },
    meta = { type = "object", additionalProperties = { type = "string" } },
  },
  required = { "mode" },
  additionalProperties = false,
}
local tools_svc = C.cordis:get("tools")
tools_svc:register("v9_edge", {
  description = "edge schema",
  side_effect = "read",
  parameters = edge_schema,
  execute = function() return "ok" end,
})
local decl = assert(tools_svc:get("v9_edge"))
local req_tools = { { name = "v9_edge", description = decl.description, parameters = decl.parameters } }
local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }

local guarded_hits = 0
local tamper = true
C.cordis:on("provider/request", function(w, next)
  if tamper then
    if type(w.body.tools) == "table" then
      if type(w.body.tools[1]) == "table" and type(w.body.tools[1].functionDeclarations) == "table" then
        w.body.tools[1].functionDeclarations[1].parametersJsonSchema.type = "string"
      elseif type(w.body.tools[1]) == "table" and type(w.body.tools[1]["function"]) == "table" then
        w.body.tools[1]["function"].parameters.type = "string"
      elseif type(w.body.tools[1]) == "table" and type(w.body.tools[1].input_schema) == "table" then
        w.body.tools[1].input_schema.type = "string"
      elseif type(w.body.tools[1]) == "table" and type(w.body.tools[1].parameters) == "table" then
        w.body.tools[1].parameters.type = "string"
      end
    end
  end
  return next(w)
end)

for _, ref in ipairs({ "openai/gpt-6.1-terra", "deepseek/deepseek-chat", "anthropic/claude-opus-5-5", "google/gemini-2.5-pro" }) do
  tamper = true
  local m_bad = LLM:stream({ model = LLM:resolve(ref), messages = msgs, tools = req_tools, no_fallback = true }, function() end)
  assert(m_bad.stop_reason == "error", "tampering tool declarations in provider/request must be blocked on " .. ref)
  tamper = false
  local m_ok = LLM:stream({ model = LLM:resolve(ref), messages = msgs, tools = req_tools, no_fallback = true }, function() end)
  assert(m_ok.stop_reason ~= "error", "valid edge schema must succeed on " .. ref .. ": " .. tostring(m_ok.error))
end
return true
"#,
        )
        .await;
        assert!(edge_ok);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn user_plugin_can_call_public_json_and_text_primitives_without_schema_meta_plant() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("probe-j.lua"),
            r#"
return {
	name = "probe-j",
	apply = function()
		assert(mare.json._clone_lossless == nil, "_clone_lossless must not exist")
		assert(mare.text._patch_paths_resolved == nil, "_patch_paths_resolved must not exist")
		assert(package.loaded["llm._schema_meta"] == nil, "llm._schema_meta plant must not exist")
		local ok_meta = pcall(require, "llm._schema_meta")
		assert(not ok_meta, "llm._schema_meta stub must not exist")

		local cloned = mare.json.clone({ a = 1, empty = mare.json.array(), n = mare.json.null })
		assert(cloned.a == 1 and mare.json.is_array(cloned.empty) and cloned.n == mare.json.null)

		local compiled = mare.schema.compile({
			type = "object",
			["x-mare-order"] = { "b", "a" },
			properties = { b = { type = "string" }, a = { type = "string" } },
		})
		local src = compiled:value()
		local dst = {
			type = "object",
			["x-mare-order"] = { "b", "a" },
			properties = { b = { type = "string" }, a = { type = "string" } },
		}
		assert(mare.json.encode(dst):find('"x-mare-order"', 1, true) ~= nil)
		mare.json.copy_metadata(src, dst)
		assert(mare.json.encode(dst) == mare.json.encode(src))
		assert(mare.json.encode(dst):find('"x-mare-order"', 1, true) == nil)

		assert(mare.text.closest("modle", { "model", "effort" }) == "model")
		assert(mare.text.closest("modle", { model = true, effort = true }) == "model")

		local patch = "*** Begin Patch\n*** Update File: rel/file.txt\n@@\n-a\n+b\n*** End Patch"
		local raw = assert(mare.text.patch_paths(patch))
		assert(raw[1].path == "rel/file.txt")
		local resolved = assert(mare.text.patch_paths(patch, { resolve = true }))
		local resolved_norm = resolved[1].path:gsub("\\", "/")
		assert(resolved_norm:sub(-#"rel/file.txt") == "rel/file.txt" and resolved_norm ~= "rel/file.txt")

		local S = require("mare_lib.schema_sanitize")
		local sanitized = S.sanitize(src, "deepseek", "https://api.deepseek.com/v1", "deepseek-chat")
		assert(mare.json.encode(sanitized) == mare.json.encode(src))
		mare.probe_j_ok = true
	end,
}
"#,
        )
        .unwrap();
        let h = env.host().user_plugins().no_session().build().await;
        let ok: bool = h.lua().load("return mare.probe_j_ok == true").eval().unwrap();
        assert!(ok, "probe-j user plugin must succeed");
        h.shutdown().await;
    })
    .await;
}

/// Issue #190: local schema validation accepts `enum` on `type: "array"`, so the Kimi/Moonshot
/// sanitizer must adapt it instead of forwarding `enum` on an array node unchanged.
#[tokio::test]
async fn kimi_schema_sanitize_adapts_array_enum() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let schema_json = json!({
            "type": "object",
            "properties": {
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "enum": [["alpha"], ["beta"]]
                }
            }
        });
        // Confirm local tool schema validation accepts this schema and enforces the array enum.
        let compiled =
            mare_shared::schema::Schema::compile(schema_json.clone()).expect("local schema accepts array enum");
        assert!(compiled.validate(&json!({ "tags": ["alpha"] })).is_ok());
        assert!(compiled.validate(&json!({ "tags": ["gamma"] })).is_err());

        let out = lua_call(
            &h,
            r#"
local S = require("mare_lib.schema_sanitize")
local src = ...
return S.sanitize(src, "kimi-for-coding", "https://api.kimi.com/coding/v1", "kimi-k2.5")
"#,
            json!([schema_json]),
        )
        .await;
        let tags = &out["properties"]["tags"];
        assert!(
            !(tags["type"] == "array" && tags.get("enum").is_some()),
            "Kimi sanitizer passed `enum` on `type: array` through unchanged: {tags}"
        );
        assert_eq!(tags["type"], "array", "array type should be preserved: {tags}");
        assert_eq!(tags["items"], json!({ "type": "string" }), "items schema should be preserved: {tags}");
        let desc = tags["description"].as_str().unwrap_or("");
        assert!(
            desc.contains("[\"alpha\"]") && desc.contains("[\"beta\"]"),
            "adapted array enum should preserve allowed values in description: {tags}"
        );

        // Structured output (`output_schema = true`, as used by provider-openai-compat response_format)
        // must preserve the array `enum` contract rather than downgrading it to prose.
        let out_schema = lua_call(
            &h,
            r#"
local S = require("mare_lib.schema_sanitize")
local src = ...
local d = S.detect_dialect("kimi-for-coding", "https://api.kimi.com/coding/v1", "kimi-k2.5", { output_schema = true })
return S.sanitize(src, d)
"#,
            json!([schema_json]),
        )
        .await;
        let out_tags = &out_schema["properties"]["tags"];
        assert_eq!(
            out_tags["enum"],
            json!([["alpha"], ["beta"]]),
            "Kimi output_schema must preserve array enum constraint: {out_tags}"
        );
        h.shutdown().await;
    })
    .await;
}
