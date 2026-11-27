//! MCP `codemode` tier: top-level isolation, disabled/approval filtering, and deferred fallback.

use super::*;

#[tokio::test]
async fn mcp_codemode_tier_isolates_top_level_and_obeys_disabled_list() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "cm");
        let cfg = servers(json!({
            "cm": with(
                fake.entry(json!({ "tools": ["echo", "direct_one", "blocked_one"] })),
                json!({
                    "tier": "codemode",
                    "tools": { "direct_one": "direct" },
                    "disabled_tools": ["blocked_one"],
                }),
            ),
        }));
        let h = env.host().builtins(&["mcp", "lua-console", "cells"]).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "M", h.service_table("mcp").unwrap());
        global(&h, "SB", h.service_table("cells").unwrap());

        wait_until(15, "direct tool from codemode server registered", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__cm__direct_one') ~= nil").await }
        })
        .await;

        let (before_direct, before_cm, before_blocked): (bool, bool, bool) = eval(
            &h,
            r#"local top = {}
               for _, d in ipairs(T:definitions()) do top[d.name] = true end
               return top.mcp__cm__direct_one == true, top.mcp__cm__echo == true, top.mcp__cm__blocked_one == true"#,
        )
        .await;
        assert!(before_direct);
        assert!(!before_cm);
        assert!(!before_blocked);

        let (mcp_cat, sb_has_cm, sb_has_blocked): (Vec<String>, bool, bool) = eval(
            &h,
            r#"local mc = {}
               for _, d in ipairs(M:catalog("codemode")) do mc[#mc + 1] = d.name end
               local has_cm, has_blocked = false, false
               for _, d in ipairs(SB:list_tools().data) do
                 if d.name == "mcp__cm__echo" then has_cm = true end
                 if d.name == "mcp__cm__blocked_one" then has_blocked = true end
               end
               return mc, has_cm, has_blocked"#,
        )
        .await;
        assert_eq!(mcp_cat, vec!["mcp__cm__echo"]);
        assert!(sb_has_cm);
        assert!(!sb_has_blocked);

        let (after_top_cm, explicit_cm, prompt_has_cm): (bool, bool, bool) = eval(
            &h,
            r#"local top = {}
               for _, d in ipairs(T:definitions()) do top[d.name] = true end
               local explicit = false
               for _, d in ipairs(T:definitions(nil, { "mcp__cm__echo" })) do
                 if d.name == "mcp__cm__echo" then explicit = true end
               end
               local ctx_svc = C.cordis:get("context")
               local sp = ctx_svc:system_prompt(C.cordis:get("llm"):current())
               return top.mcp__cm__echo == true, explicit, sp:find("from codemode scripts", 1, true) ~= nil"#,
        )
        .await;
        assert!(!after_top_cm, "codemode tool must stay excluded from top-level model declarations");
        assert!(explicit_cm, "codemode tool must be included when explicitly requested");
        assert!(prompt_has_cm, "system prompt must describe codemode server");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mcp_codemode_tier_falls_back_to_deferred_without_executor() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "cm_fallback");
        let cfg = json!({
            "plugins": {
                "cells": { "enabled": false },
                "mcp": {
                    "servers": {
                        "cm": with(fake.entry(json!({ "tools": ["echo"] })), json!({ "tier": "codemode" })),
                    }
                }
            }
        });
        let h = env.host().builtins(&["mcp", "lua-console", "cells"]).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "M", h.service_table("mcp").unwrap());

        let (cat_len, has_search, prompt_deferred): (usize, bool, bool) = eval(
            &h,
            r#"local cat = M:catalog("codemode")
               local ctx_svc = C.cordis:get("context")
               local sp = ctx_svc:system_prompt(C.cordis:get("llm"):current())
               return #cat, T:get("tool_search") ~= nil, sp:find("find its tools with tool_search", 1, true) ~= nil"#,
        )
        .await;
        assert_eq!(cat_len, 0);
        assert_eq!(fake.count("start"), 0, "catalog(codemode) must not start server without executor");
        assert!(has_search, "tool_search must be registered when codemode falls back to deferred");
        assert!(prompt_deferred, "system prompt must fall back to tool_search hint");

        let loaded: String = eval(
            &h,
            r#"local r = T:execute({ id = "s1", name = "tool_search", arguments = { query = "echo" } })
               return tostring(r.content)"#,
        )
        .await;
        assert!(loaded.contains("mcp__cm__echo"), "{loaded}");

        h.shutdown().await;
    })
    .await
}
