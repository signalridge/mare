//! Generic deferred tools and their search sources (pi agent-session-mcp.test.ts:889,975).

use super::*;
use crate::suite::kernel::write_plugin;

const DEFERRED_TOOL: &str = r#"return { name = "deferred-test", inject = { "tools" }, apply = function(ctx)
  ctx.tools:register("mytool", { description = "A plugin tool", exposure = "deferred",
    search = { aliases = { "raw" }, text = "extra words" }, side_effect = "read",
    parameters = { type = "object", properties = {} },
    execute = function() return { content = "mytool ok" } end })
end }"#;

const SOURCE: &str = r#"return { name = "source-test", inject = { "tools" }, apply = function(ctx)
  mare._prepares = {}
  if ctx.tools.register_search_source then
    mare._remove_first = ctx.tools:register_search_source({ prepare = function()
      table.insert(mare._prepares, "first")
      error("source failed")
    end })
    mare._remove_second = ctx.tools:register_search_source({ prepare = function()
      table.insert(mare._prepares, "second")
      mare._remove_tool = ctx.tools:register("late_tool", { description = "Late tool", exposure = "deferred",
        side_effect = "read", parameters = { type = "object", properties = {} },
        execute = function() return { content = "late ok" } end })
    end })
  end
end }"#;

const DEFINITION_NAMES: &str = r#"local names = {}
  for _, d in ipairs(T:definitions({})) do names[d.name] = true end
  return names.mytool == true, names.tool_search == true"#;

#[tokio::test]
async fn a_plugin_tool_with_deferred_exposure_is_found_and_keeps_declarations_stable() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "deferred-test", DEFERRED_TOOL);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        let before: (bool, bool) = eval(&h, DEFINITION_NAMES).await;
        assert_eq!(before, (false, true), "only tool_search is initially declared on non-native model");
        let native: bool = eval(
            &h,
            r#"for _, d in ipairs(T:definitions({ tool_update = "addition_only" })) do
          if d.name == "mytool" then return d.defer_loading == true end end return false"#,
        )
        .await;
        assert!(native, "native tool_update models receive the tool with defer_loading");
        exec(&h, r#"C.cordis:on("tools/loaded", function(ev) mare._loaded = ev.tools end)"#).await;
        let generation: i64 = eval(&h, "return T:revision()").await;
        let found = call(&h, "tool_search", json!({ "query": "mytool" })).await;
        assert_eq!(found["details"]["loaded_tools"], json!(["mytool"]), "{found}");
        let (loaded, next_generation): (String, i64) = eval(&h, "return mare._loaded[1], T:revision()").await;
        assert_eq!(loaded, "mytool");
        assert_eq!(next_generation, generation, "tool_search must not bump tools revision");
        let after: (bool, bool) = eval(&h, DEFINITION_NAMES).await;
        assert_eq!(after, (false, true), "non-native top-level declarations stay unchanged after tool_search");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tool_search_is_not_declared_when_nothing_is_deferred() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let (_, search): (bool, bool) = eval(&h, DEFINITION_NAMES).await;
        assert!(!search, "tool_search needs a deferred tool or source");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn search_sources_prepare_before_the_catalog_is_read() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "source-test", SOURCE);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        let (_, search): (bool, bool) = eval(&h, DEFINITION_NAMES).await;
        assert!(search, "a search source declares tool_search before it supplies any tools");
        let found = call(&h, "tool_search", json!({ "query": "late_tool" })).await;
        assert_eq!(found["details"]["loaded_tools"], json!(["late_tool"]), "{found}");
        let order: String = eval(&h, "return table.concat(mare._prepares, ',')").await;
        assert_eq!(order, "first,second", "a failed source does not prevent later sources from preparing");
        exec(&h, "mare._remove_first(); mare._remove_second(); mare._remove_tool()").await;
        let (_, search): (bool, bool) = eval(&h, DEFINITION_NAMES).await;
        assert!(!search, "removing all sources and tools withdraws tool_search");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn select_matches_search_aliases() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "deferred-test", DEFERRED_TOOL);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        let found = call(&h, "tool_search", json!({ "query": "select:raw" })).await;
        assert_eq!(found["details"]["loaded_tools"], json!(["mytool"]), "{found}");
        let by_text = call(&h, "tool_search", json!({ "query": "extra" })).await;
        assert_eq!(by_text["details"]["loaded_tools"], json!(["mytool"]), "{by_text}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tool_search_description_does_not_name_mcp() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "deferred-test", DEFERRED_TOOL);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        let (non_native, native): (String, String) = eval(
            &h,
            r#"local function desc_for(m)
                 for _, d in ipairs(T:definitions(m)) do
                   if d.name == "tool_search" then return d.description end
                 end
                 return ""
               end
               return desc_for({}), desc_for({ tool_update = "addition_only" })"#,
        )
        .await;
        assert!(non_native.contains("call_tool"), "non-native description should mention call_tool: {non_native}");
        assert!(native.contains("directly"), "native description should mention direct calls: {native}");
        assert!(!non_native.to_lowercase().contains("mcp"), "{non_native}");
        assert!(!native.to_lowercase().contains("mcp"), "{native}");
        h.shutdown().await;
    })
    .await
}

/// A deferred tool the user turned off is never declared, so tool_search does not report it as
/// loaded (Kimi Code packages/agent-core-v2/test/agent/toolSelect/toolSelectService.test.ts:1291
/// reports such names as policy-denied instead of selected).
#[tokio::test]
async fn tool_search_does_not_load_a_tool_the_user_disabled() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "deferred-test", DEFERRED_TOOL);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        exec(&h, r#"T:disable("mytool")"#).await;
        let found = call(&h, "tool_search", json!({ "query": "select:mytool" })).await;
        assert!(found["details"]["loaded_tools"].is_null(), "{found}");
        assert!(found["content"].as_str().unwrap_or_default().starts_with("No tools match"), "{found}");
        let (_, search): (bool, bool) = eval(&h, DEFINITION_NAMES).await;
        assert!(!search, "a disabled deferred tool alone does not declare tool_search");
        h.shutdown().await;
    })
    .await
}

/// `select:` loads every named tool; `max_results` limits keyword search only (as before TOOL-DEFER).
#[tokio::test]
async fn select_loads_every_named_tool_past_the_result_limit() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(
            &env,
            "many-deferred",
            r#"return { name = "many-deferred", inject = { "tools" }, apply = function(ctx)
  for i = 1, 6 do
    ctx.tools:register("t" .. i, { description = "tool " .. i, exposure = "deferred", side_effect = "read",
      parameters = { type = "object", properties = {} }, execute = function() return { content = "ok" } end })
  end
end }"#,
        );
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        let found = call(&h, "tool_search", json!({ "query": "select:t1,t2,t3,t4,t5,t6" })).await;
        assert_eq!(found["details"]["loaded_tools"], json!(["t1", "t2", "t3", "t4", "t5", "t6"]), "{found}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tools_prompt_lists_deferred_names_deterministically_and_preserves_native_cache_prefix() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(
            &env,
            "deferred-prompt-test",
            r#"return { name = "deferred-prompt-test", inject = { "tools" }, apply = function(ctx)
  ctx.tools:register("z_deferred", {
    description = "Zeta tool",
    snippet = "Should not leak into direct snippet list",
    exposure = "deferred",
    side_effect = "read",
    parameters = { type = "object", properties = {} },
    execute = function() return { content = "z ok" } end,
  })
  ctx.tools:register("a_deferred", {
    description = "Alpha tool",
    exposure = "deferred",
    side_effect = "read",
    parameters = { type = "object", properties = {} },
    execute = function() return { content = "a ok" } end,
  })
end }"#,
        );
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        let before_non_native: String =
            eval(&h, "return C.cordis:get('context'):system_prompt({ ref = 'test/non-native' })").await;
        assert!(
            before_non_native.contains("- tool_search: Discover and load deferred tools before calling them"),
            "tool_search must appear with its snippet in <tools>: {before_non_native}"
        );
        assert!(
            before_non_native.contains("- call_tool: Invoke a deferred tool by name and arguments"),
            "call_tool must appear with its snippet on non-native models: {before_non_native}"
        );
        assert!(
            !before_non_native.contains("- z_deferred:"),
            "deferred tool with snippet must not appear as a direct tool snippet: {before_non_native}"
        );
        assert!(
            before_non_native.contains(
                "Deferred tools (call tool_search with keywords or select:<name1,name2> for schemas, then invoke via call_tool): a_deferred, z_deferred"
            ),
            "non-native deferred tools must use call_tool phrasing in sorted order: {before_non_native}"
        );
        assert!(
            !before_non_native.contains("Additional tools may be available."),
            "explicit deferred list replaces generic line: {before_non_native}"
        );

        let before_native: String = eval(
            &h,
            "return C.cordis:get('context'):system_prompt({ ref = 'test/native', tool_update = 'addition_only' })",
        )
        .await;
        assert!(
            !before_native.contains("- call_tool:"),
            "native models must not advertise call_tool in <tools>: {before_native}"
        );
        assert!(
            before_native.contains(
                "Deferred tools (call tool_search with keywords or select:<name1,name2> to load, then invoke directly): a_deferred, z_deferred"
            ),
            "native deferred tools must use direct phrasing in sorted order: {before_native}"
        );

        call(&h, "tool_search", json!({ "query": "select:a_deferred" })).await;
        // Force a prompt rebuild to verify the rendered `<tools>` section itself (not only the cache).
        exec(&h, "C.cordis:get('context'):invalidate()").await;

        let after_native: String = eval(
            &h,
            "return C.cordis:get('context'):system_prompt({ ref = 'test/native', tool_update = 'addition_only' })",
        )
        .await;
        assert_eq!(
            after_native, before_native,
            "native tool_update path must not mutate the system prompt prefix when tools load"
        );

        let after_non_native: String =
            eval(&h, "return C.cordis:get('context'):system_prompt({ ref = 'test/non-native' })").await;
        assert_eq!(
            after_non_native, before_non_native,
            "non-native path must also keep the deferred tools list stable across tool_search"
        );

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tool_search_respects_budget_degrades_progressively_and_caps_select_at_32() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(
            &env,
            "budget-deferred",
            r#"return { name = "budget-deferred", inject = { "tools" }, apply = function(ctx)
  local long_desc = string.rep("detailed description text ", 120)
  for i = 1, 40 do
    local name = string.format("btool_%02d", i)
    ctx.tools:register(name, {
      description = name .. " " .. long_desc,
      exposure = "deferred",
      side_effect = "read",
      parameters = {
        type = "object",
        properties = {
          payload = { type = "string", description = string.rep("schema field documentation ", 60) },
          extra = { type = "integer", description = "Extra parameter for " .. name },
        },
        required = { "payload" },
      },
      execute = function() return { content = "ok" } end,
    })
  end
  -- Also register 5 tools with huge descriptions but tiny schemas to verify Pass 2 (description shortening)
  -- keeps all schemas before Pass 3 drops schemas.
  for i = 1, 5 do
    local name = string.format("dtool_%02d", i)
    ctx.tools:register(name, {
      description = name .. " " .. string.rep("verbose narrative ", 400),
      exposure = "deferred",
      side_effect = "read",
      parameters = { type = "object", properties = { id = { type = "string" } }, required = { "id" } },
      execute = function() return { content = "ok" } end,
    })
  end
end }"#,
        );
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);

        // Pass 2: long descriptions shortened while keeping all 5 schemas within 6000 tokens (24000 bytes).
        let pass2 =
            call(&h, "tool_search", json!({ "query": "select:dtool_01,dtool_02,dtool_03,dtool_04,dtool_05" })).await;
        let pass2_text = text(&pass2);
        assert!(pass2_text.len() <= 24000, "pass 2 result exceeded 6000-token budget: {} bytes", pass2_text.len());
        assert_eq!(
            pass2["details"]["loaded_tools"],
            json!(["dtool_01", "dtool_02", "dtool_03", "dtool_04", "dtool_05"]),
            "pass 2 must keep all 5 tools loaded after shortening descriptions"
        );
        assert!(pass2_text.contains("..."), "pass 2 should shorten long descriptions");
        assert!(pass2_text.contains("parameters:"), "pass 2 must include full parameter schemas");
        assert!(!pass2_text.contains("Additional matching tools"), "pass 2 should not drop schemas");

        // Pass 3 + select: > 32 cap: requesting 40 large-schema tools caps select at 32,
        // keeps output <= 6000 tokens, includes full schema only for leading K, and sets loaded_tools == K.
        let all_40: Vec<String> = (1..=40).map(|i| format!("btool_{i:02}")).collect();
        let pass3 = call(&h, "tool_search", json!({ "query": format!("select:{}", all_40.join(",")) })).await;
        let pass3_text = text(&pass3);
        assert!(pass3_text.len() <= 24000, "pass 3 result exceeded 6000-token budget: {} bytes", pass3_text.len());
        let loaded = pass3["details"]["loaded_tools"].as_array().expect("loaded_tools array");
        assert!(!loaded.is_empty() && loaded.len() < 32, "expected budget to trim full schemas: len={}", loaded.len());
        assert_eq!(
            pass3_text.matches("  parameters: ").count(),
            loaded.len(),
            "loaded_tools must match the exact set of tools whose full schema was rendered"
        );
        assert!(
            pass3_text.contains("Additional matching tools (omitted to fit budget"),
            "pass 3 must list omitted tool names with select hint: {pass3_text}"
        );
        assert!(
            pass3_text.contains("returned the first 32 of 40 matching tools"),
            "select: > 32 must append truncation note: {pass3_text}"
        );
        assert!(!pass3_text.contains("btool_33"), "tools past the 32 select cap must not be returned");

        h.shutdown().await;
    })
    .await
}

/// When the leading matched tool's schema alone exceeds the 6,000-token (24,000-byte) budget,
/// `tool_search` must keep 0 full schemas, emit a bounded explanation within budget, bound omitted
/// names, and avoid marking the oversized tool loaded.
#[tokio::test]
async fn tool_search_oversized_single_schema_stays_within_budget_and_loads_nothing() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(
            &env,
            "oversized-deferred",
            r#"return { name = "oversized-deferred", inject = { "tools" }, apply = function(ctx)
  local giant_props = {}
  for i = 1, 80 do
    giant_props[string.format("field_%03d", i)] = {
      type = "string",
      description = string.rep("very long parameter schema documentation ", 12),
    }
  end
  ctx.tools:register("giant_schema_tool", {
    description = "Tool whose parameter schema alone exceeds 24000 bytes",
    exposure = "deferred",
    side_effect = "read",
    parameters = { type = "object", properties = giant_props },
    execute = function() return { content = "ok" } end,
  })
  for i = 1, 25 do
    local long_name = string.format("extra_deferred_tool_%02d_%s", i, string.rep("x", 36))
    ctx.tools:register(long_name, {
      description = "Extra match " .. i,
      exposure = "deferred",
      side_effect = "read",
      parameters = { type = "object", properties = {} },
      execute = function() return { content = "ok" } end,
    })
  end
end }"#,
        );
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        exec(&h, r#"LOADED_EVENTS = 0; C.cordis:on("tools/loaded", function() LOADED_EVENTS = LOADED_EVENTS + 1 end)"#)
            .await;

        let single = call(&h, "tool_search", json!({ "query": "select:giant_schema_tool" })).await;
        let single_text = text(&single);
        assert!(single_text.len() <= 24000, "oversized single hit exceeded 24000 bytes: {}", single_text.len());
        assert_eq!(single["details"]["loaded_tools"], json!([]), "oversized tool must not be marked loaded: {single}");
        assert!(!single_text.contains("  parameters: "), "must not render oversized schema: {single_text}");
        assert!(
            single_text.contains("giant_schema_tool") && single_text.contains("budget"),
            "must explain why no schema was loaded: {single_text}"
        );
        assert_eq!(eval::<i64>(&h, "return LOADED_EVENTS").await, 0, "must not emit tools/loaded for empty load");

        let mut many_names = vec!["giant_schema_tool".to_string()];
        for i in 1..=25 {
            many_names.push(format!("extra_deferred_tool_{i:02}_{}", "x".repeat(36)));
        }
        let many = call(&h, "tool_search", json!({ "query": format!("select:{}", many_names.join(",")) })).await;
        let many_text = text(&many);
        assert!(many_text.len() <= 24000, "oversized multi hit exceeded 24000 bytes: {}", many_text.len());
        assert_eq!(many["details"]["loaded_tools"], json!([]), "{many}");
        assert!(
            many_text.contains("... (+") && many_text.contains("more)"),
            "omitted names list must be bounded with overflow indicator: {many_text}"
        );

        h.shutdown().await;
    })
    .await
}
