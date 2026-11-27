//! The system prompt section registry: a name registered twice in
//! one layer is an error, a nearer layer shadows a farther one, `{{var}}` interpolation is strict,
//! two complete sections fail the assembly, a sub-agent's own sections live in its agent layer,
//! and reloading a plugin re-registers its sections without a duplicate.

use crate::common::*;
use mare_shared::ui::Mode;
use serde_json::json;

const MODEL: &str = r#"{ id = "echo", provider = "faux", ref = "faux/echo", name = "Faux Echo" }"#;

/// A host with `CX` (the context service as the test ctx `C` reaches it) exposed.
async fn host(env: &TestEnv) -> std::sync::Arc<mare_host::Host> {
    let h = env.host().all_builtins().faux(json!([{ "text": "answered" }])).no_session().build().await;
    expose(&h);
    exec(&h, "CX = C.cordis:get('context')").await;
    h
}

/// `pcall(system_prompt)` after `setup` (Lua): (ok, prompt or error message)
async fn try_prompt(h: &mare_host::Host, setup: &str) -> (bool, String) {
    eval(
        h,
        &format!(
            r#"{setup}
               CX:invalidate()
               local ok, r = pcall(CX.system_prompt, CX, {MODEL})
               return ok, tostring(r)"#
        ),
    )
    .await
}

#[tokio::test]
async fn a_name_registered_twice_in_one_layer_is_an_error() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        // `tools` is the tools plugin's section (global layer)
        let err: String = eval(
            &h,
            r#"local ok, e = pcall(CX.section, CX, { name = "tools", text = "MINE" }) return tostring(e)"#,
        )
        .await;
        assert!(err.contains(r#"prompt section "tools" is registered twice in the global layer (by tools and"#), "{err}");
        // `persona` collides in the global layer; removed legacy name `preamble` is not an alias
        let err: String = eval(
            &h,
            r#"local ok, e = pcall(CX.section, CX, { name = "persona", text = "MINE" }) return tostring(e)"#,
        )
        .await;
        assert!(err.contains(r#"prompt section "persona" is registered twice"#), "{err}");
        let preamble_distinct: bool = eval(
            &h,
            r#"
            local rel = CX:section({ name = "preamble", text = "DISTINCT" })
            rel()
            return true
            "#,
        )
        .await;
        assert!(preamble_distinct, "preamble must no longer alias persona");
        // reverse: the user layer shadows the plugin's section instead of failing
        let (ok, text) = try_prompt(&h, r#"CX:section({ name = "tools", order = 100, text = "USER TOOLS", scope = "user" })"#).await;
        assert!(ok, "{text}");
        assert!(text.contains("<tools>\nUSER TOOLS\n</tools>") && !text.contains("- read: Read file contents"), "{text}");
        // and twice in the user layer is an error again
        let err: String = eval(
            &h,
            r#"local ok, e = pcall(CX.section, CX, { name = "tools", text = "AGAIN", scope = "user" }) return tostring(e)"#,
        )
        .await;
        assert!(err.contains("registered twice in the user layer"), "{err}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn interpolation_is_strict() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let (ok, err) = try_prompt(&h, r#"CX:section({ name = "probe", text = "x {{nope}} y" })"#).await;
        assert!(!ok && err.contains(r#"unknown prompt variable {{nope}} in section "probe""#), "{err}");
        // the request is refused before it is sent, naming the section and the variable
        let res = h.prompt("hello", json!({})).await.unwrap();
        assert_eq!(res["not_sent"], true, "{res}");
        let reason = res["error"]["message"].as_str().unwrap();
        assert!(reason.contains("{{nope}}") && reason.contains("probe"), "{reason}");
        h.shutdown().await;

        let h = host(&env).await;
        let (ok, err) = try_prompt(&h, r#"CX:section({ name = "probe", text = "{{Bad-Name}}" })"#).await;
        assert!(!ok && err.contains(r#"malformed prompt variable {{Bad-Name}} in section "probe""#), "{err}");
        h.shutdown().await;

        let h = host(&env).await;
        let (ok, err) = try_prompt(&h, r#"CX:section({ name = "probe", text = "{{}}" })"#).await;
        assert!(!ok && err.contains("malformed prompt variable {{}}"), "{err}");
        h.shutdown().await;

        // reverse checks: known variables are replaced, a lone `{{` is text, values aren't
        // scanned again, and `interpolate = false` passes the text through
        let h = host(&env).await;
        let (ok, text) = try_prompt(
            &h,
            r#"CX:variable("probe_value", function() return "{{model}}" end)
               CX:section({ name = "probe", text = "M={{model}} P={{provider}} V={{probe_value}} open {{ end" })
               CX:section({ name = "verbatim", text = "{{nope}}", interpolate = false })"#,
        )
        .await;
        assert!(ok, "{text}");
        assert!(text.contains("<probe>\nM=echo P=faux V={{model}} open {{ end\n</probe>"), "{text}");
        assert!(text.contains("<verbatim>\n{{nope}}\n</verbatim>"), "{text}");
        // a variable registered twice is an error too
        let err: String =
            eval(&h, r#"local ok, e = pcall(CX.variable, CX, "model", function() return "x" end) return tostring(e)"#)
                .await;
        assert!(err.contains(r#"prompt variable "model" is registered twice"#), "{err}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn complete_sections_are_deterministic() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        // one complete section is the whole prompt
        let (ok, text) = try_prompt(&h, r#"CX:section({ name = "whole", text = "ONLY THIS", complete = true })"#).await;
        assert!(ok && text == "ONLY THIS", "{text}");
        // a second active one fails the assembly, whatever the table order
        let (ok, err) = try_prompt(&h, r#"CX:section({ name = "whole2", text = "OR THIS", complete = true })"#).await;
        assert!(!ok && err.contains(r#"two complete prompt sections are active: "whole" and "whole2""#), "{err}");
        // reverse: an inactive (`when` false or empty) complete section doesn't count
        let (ok, text) = try_prompt(&h, r#"CX:suppress("whole2")"#).await;
        assert!(ok && text == "ONLY THIS", "{text}");
        let (ok, text) = try_prompt(
            &h,
            r#"CX:suppress("whole")
               CX:section({ name = "off", text = "OFF", complete = true, when = function() return false end })
               CX:section({ name = "empty", text = "", complete = true })"#,
        )
        .await;
        assert!(ok && text.contains("<tools>") && !text.contains("OFF"), "{text}");
        h.shutdown().await;
    })
    .await
}

/// A sub-agent's `persona` / `sections` / `system` (spawn options, `mare.args` in its VM) shadow
/// the global sections in its own prompt only; the main session's prompt is unchanged.
#[tokio::test]
async fn a_sub_agent_has_its_own_scoped_sections() {
    local(async {
        let env = TestEnv::new().await;
        let main = host(&env).await;
        let (ok, main_text) = try_prompt(&main, "").await;
        assert!(ok, "{main_text}");
        main.shutdown().await;

        let child = |args: serde_json::Value| {
            let mut b = env.host().all_builtins().faux(json!([])).args(args);
            b.opts.mode = Mode::Child;
            b
        };
        let h = child(json!({
            "no_session": true, "agent_label": "worker",
            "persona": "CHILD PERSONA for {{model}}",
            "sections": [
                { "name": "rules", "order": 200, "text": "CHILD RULES" },
                { "name": "extra", "order": 50, "text": "EXTRA" }
            ]
        }))
        .build()
        .await;
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());
        exec(&h, "CX = C.cordis:get('context')").await;
        let (ok, text) = try_prompt(&h, "").await;
        assert!(ok, "{text}");
        // `extra` sorts beside the default `approach` (both order 50, then by tag)
        assert!(text.starts_with("CHILD PERSONA for echo\n\n<approach>"), "{text}");
        assert!(text.contains("</approach>\n\n<extra>\nEXTRA\n</extra>\n\n<deliverables>"), "{text}");
        assert!(text.contains("<rules>\nCHILD RULES\n</rules>") && !text.contains("Be concise"), "{text}");
        assert!(!text.contains("You are a coding agent"), "{text}");
        // a named-agent section applies to that agent only; this VM's prompt is built for its own
        let (ok, text) = try_prompt(&h, r#"CX:section({ name = "named", agent = "other", text = "FOR OTHER" })"#).await;
        assert!(ok && !text.contains("FOR OTHER"), "{text}");
        let other: String = eval(&h, &format!("return (CX:system_prompt({MODEL}, 'other'))")).await;
        assert!(other.contains("FOR OTHER"), "{other}");
        h.shutdown().await;

        // `system` is the whole prompt, verbatim
        let h = child(json!({ "no_session": true, "system": "WHOLE {{x}}" })).build().await;
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());
        exec(&h, "CX = C.cordis:get('context')").await;
        let (ok, text) = try_prompt(&h, "").await;
        assert!(ok && text == "WHOLE {{x}}", "{text}");
        h.shutdown().await;

        // reverse: the main session's prompt doesn't carry any of it
        let main = host(&env).await;
        let (ok, again) = try_prompt(&main, "").await;
        assert!(ok && again == main_text, "{again}");
        assert!(!again.contains("CHILD") && !again.contains("EXTRA"), "{again}");
        main.shutdown().await;
    })
    .await
}

/// Reloading a section's owner withdraws and re-registers it (no duplicate error), and
/// `/reload context` brings back the sections of plugins it doesn't reload (tools, permissions).
#[tokio::test]
async fn reloads_re_register_sections() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .all_builtins()
            .faux(json!([]))
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "auto" } } }))
            .args(json!({ "no_session": true }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        let prompt = |h: std::sync::Arc<mare_host::Host>| async move {
            global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());
            exec(&h, "CX = C.cordis:get('context')").await;
            let (ok, text) = try_prompt(&h, "").await;
            assert!(ok, "{text}");
            text
        };
        let before = prompt(h.clone()).await;
        assert!(before.contains("<tools>") && before.contains("<sandbox>"), "{before}");
        for name in ["context", "tools", "permissions", "tools-apply-patch", "user-config", "context"] {
            let report = h.kernel.reload_report(Some(name)).await.unwrap();
            assert!(report.failures.is_empty(), "reloading {name}: {:?}", report.failures);
            if name == "context" {
                // `/reload context` doesn't drag in tools (its section comes back through
                // its `inject`); permissions is reloaded through `session` as before
                assert!(!report.applied.iter().any(|n| n == "tools"), "{:?}", report.applied);
            }
            let after = prompt(h.clone()).await;
            assert_eq!(after, before, "after reloading {name} ({:?})", report.applied);
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn user_tool_description_is_applied_at_startup() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), r#"mare.tool.describe("bash", "user startup description")"#)
            .unwrap();
        let h = env.host().user_config(true).faux(json!([])).no_session().build().await;
        expose(&h);
        let description: String = eval(&h, r#"return T:definitions({}, { "bash" })[1].description"#).await;
        assert_eq!(description, "user startup description", "before any reload");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn user_bash_description_survives_startup_without_jobs() {
    local(async {
        for plugins in [
            json!({ "preset": "minimal", "user-config": { "enabled": true } }),
            json!({ "jobs": { "enabled": false } }),
        ] {
            let env = TestEnv::new().await;
            std::fs::create_dir_all(env.config_dir()).unwrap();
            std::fs::write(
                env.config_dir().join("init.lua"),
                r#"mare.tool.describe("bash", "user shell description")"#,
            )
            .unwrap();
            let h = env
                .host()
                .user_config(true)
                .faux(json!([]))
                .config(json!({ "plugins": plugins }))
                .no_session()
                .build_exposed()
                .await;
            let (description, has_background): (String, bool) = eval(
                &h,
                r#"
                local d = T:definitions({}, { "bash" })[1]
                return d.description, d.parameters.properties.run_in_background ~= nil
            "#,
            )
            .await;
            assert_eq!(description, "user shell description");
            assert!(!has_background, "background execution requires jobs");
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn user_tool_description_is_reapplied_after_its_owner_reloads() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["tools-apply-patch", "user-config"])
            .faux(json!([]))
            .config(json!({ "tool_descriptions": { "apply_patch": "user patch description" } }))
            .no_session()
            .build()
            .await;
        for _ in 0..2 {
            let report = h.kernel.reload_report(Some("tools-apply-patch")).await.unwrap();
            assert!(report.failures.is_empty(), "{:?}", report.failures);
            assert!(
                !report.applied.iter().any(|name| name == "tools" || name == "user-config"),
                "{:?}",
                report.applied
            );
            expose(&h);
            let description: String = eval(&h, r#"return T:get("apply_patch").description"#).await;
            assert_eq!(description, "user patch description");
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tool_schema_wrappers_follow_their_owners_across_reload() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().all_builtins().faux(json!([])).config(json!({
            "tool_descriptions": {"bash": "custom shell description"},
            "plugins": { "permissions": { "enabled": true, "preset": "auto" } }
        })).args(json!({ "no_session": true }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        for name in ["initial", "permissions", "tools", "user-config", "tools"] {
            if name != "initial" {
                let report = h.kernel.reload_report(Some(name)).await.unwrap();
                assert!(report.failures.is_empty(), "{name}: {:?}", report.failures);
            }
            expose(&h);
            let (description, fields): (String, bool) = eval(&h, r#"local d = T:get("bash")
                return d.description, d.parameters.properties.sandbox_permissions ~= nil and d.parameters.properties.justification ~= nil"#).await;
            assert_eq!(description, "custom shell description", "{name}");
            assert!(fields, "permissions schema disappeared after {name}");
        }
        h.shutdown().await;
    }).await
}

/// `context:section` returns its release: it withdraws exactly that section, once — a later
/// registration under the same name is not touched by calling it again. Ported from 68032fbb.
#[tokio::test]
async fn section_returns_an_exact_idempotent_release() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let (has_s1, gone_s1, has_s2, gone_s2): (bool, bool, bool, bool) = eval(
            &h,
            &format!(
                r#"local release = CX:section({{ name = "ephemeral", text = "FIRST" }})
                   assert(require("mare_lib").callable(release), "section must return its release")
                   local before = CX:system_prompt({MODEL})
                   release()
                   local after = CX:system_prompt({MODEL})
                   local release2 = CX:section({{ name = "ephemeral", text = "SECOND" }})
                   -- a second call of the first release must not remove the newer registration
                   release()
                   local with_second = CX:system_prompt({MODEL})
                   release2()
                   local after_second = CX:system_prompt({MODEL})
                   return before:find("FIRST", 1, true) ~= nil,
                          after:find("FIRST", 1, true) == nil,
                          with_second:find("SECOND", 1, true) ~= nil,
                          after_second:find("SECOND", 1, true) == nil"#
            ),
        )
        .await;
        assert!(has_s1 && gone_s1 && has_s2 && gone_s2, "{has_s1} {gone_s1} {has_s2} {gone_s2}");
        h.shutdown().await;
    })
    .await
}

/// `context:suppress_runtime` hides the optional runtime sections (cwd, environment) — a
/// `required` one stays — until every suppression is released, and only in its layer (an agent's
/// suppression leaves the main prompt alone). Ported from 68032fbb.
#[tokio::test]
async fn suppress_runtime_hides_the_optional_runtime_sections() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let r: (bool, bool, bool, bool, bool, bool, bool) = eval(
            &h,
            &format!(
                r#"CX:section({{ name = "req_rt", text = "REQ_RUNTIME", runtime = true, required = true }})
                   local before = CX:system_prompt({MODEL})
                   local d1 = CX:suppress_runtime()
                   local d2 = CX:suppress_runtime()
                   local sup = CX:system_prompt({MODEL})
                   d1()
                   local still = CX:system_prompt({MODEL})
                   d2()
                   local restored = CX:system_prompt({MODEL})
                   local dw = CX:suppress_runtime({{ scope = "agent", agent = "worker" }})
                   local worker_text = CX:system_prompt({MODEL}, "worker")
                   local main_text = CX:system_prompt({MODEL})
                   dw()
                   return before:find("<cwd>", 1, true) ~= nil and before:find("<environment>", 1, true) ~= nil,
                          sup:find("<cwd>", 1, true) == nil and sup:find("<environment>", 1, true) == nil,
                          sup:find("REQ_RUNTIME", 1, true) ~= nil,
                          still:find("<cwd>", 1, true) == nil,
                          restored:find("<cwd>", 1, true) ~= nil,
                          worker_text:find("<cwd>", 1, true) == nil,
                          main_text:find("<cwd>", 1, true) ~= nil"#
            ),
        )
        .await;
        assert!(r.0 && r.1 && r.2 && r.3 && r.4 && r.5 && r.6, "{r:?}");
        h.shutdown().await;
    })
    .await
}

/// `context:preview` builds the prompt with candidate sections through the live renderer and the
/// context/system post-hook, changes nothing, and reports each section's place (shadowed, whether
/// its text survived the hook); a candidate that can't be built raises at preview time — an
/// unknown {{var}}, two complete sections. Ported from 68032fbb.
#[tokio::test]
async fn preview_builds_candidates_without_publishing_and_reports_each_section() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let r: (bool, bool, bool, bool, bool, bool, bool) = eval(
            &h,
            &format!(
                r#"-- another plugin's live section (a child fiber of the test ctx)
                   C.cordis:plugin({{ name = "other-plugin", apply = function(ctx) OTHER = ctx end }})
                   while not OTHER do mare.task.sleep(5) end
                   OTHER:get("context"):section({{ name = "other_sec", text = "OTHER_LIVE" }})
                   CX:section({{ name = "persona", text = "LIVE_CALLER_PERSONA", scope = "user" }})
                   local live_before = CX:system_prompt({MODEL})
                   local p1 = CX:preview({MODEL}, nil, {{
                       replace = true,
                       suppress_runtime = true,
                       sections = {{
                           {{ name = "persona", text = "CANDIDATE_M={{{{model}}}}", scope = "user" }},
                           {{ name = "dropped_by_hook", text = "DROP_ME" }},
                       }},
                   }})
                   local live_after = CX:system_prompt({MODEL})
                   local shadow_found = false
                   for _, s in ipairs(p1.sections) do
                       if s.name == "persona" and s.layer == "global" and s.status == "shadowed"
                           and s.shadowed_by and s.shadowed_by.layer == "user" then
                           shadow_found = true
                       end
                   end
                   -- the post-hook drops one candidate's text: preserved = false for it
                   C.cordis:on("context/system", function(data, ev)
                       data.text = (data.text:gsub("<dropped_by_hook>.-</dropped_by_hook>", "")) .. "\nHOOK_TAIL"
                       return ev.next(data)
                   end)
                   local p2 = CX:preview({MODEL}, nil, {{
                       replace = true,
                       sections = {{ {{ name = "dropped_by_hook", text = "DROP_ME" }} }},
                   }})
                   local drop_preserved = true
                   for _, s in ipairs(p2.sections) do
                       if s.name == "dropped_by_hook" then
                           drop_preserved = s.preserved
                       end
                   end
                   local p3 = CX:preview({MODEL}, nil, {{
                       replace = true,
                       sections = {{ {{ name = "whole", text = "ONLY_COMPLETE", complete = true }} }},
                   }})
                   local ok_var, err_var = pcall(CX.preview, CX, {MODEL}, nil, {{
                       sections = {{ {{ name = "bad", text = "{{{{missing_var}}}}" }} }},
                   }})
                   local ok_dup, err_dup = pcall(CX.preview, CX, {MODEL}, nil, {{
                       sections = {{
                           {{ name = "c1", text = "A", complete = true }},
                           {{ name = "c2", text = "B", complete = true }},
                       }},
                   }})
                   return p1.text:find("CANDIDATE_M=echo", 1, true) ~= nil
                              and p1.text:find("OTHER_LIVE", 1, true) ~= nil
                              and p1.text:find("LIVE_CALLER_PERSONA", 1, true) == nil
                              and p1.text:find("<cwd>", 1, true) == nil
                              and p1.runtime_suppressed == true,
                          live_before == live_after and live_after:find("LIVE_CALLER_PERSONA", 1, true) ~= nil,
                          shadow_found,
                          not drop_preserved and p2.text:find("HOOK_TAIL", 1, true) ~= nil,
                          p3.text == "ONLY_COMPLETE" and p3.complete == "whole",
                          not ok_var and tostring(err_var):find("missing_var", 1, true) ~= nil,
                          not ok_dup and tostring(err_dup):find("two complete prompt sections", 1, true) ~= nil"#
            ),
        )
        .await;
        assert!(r.0, "candidates in the preview: {r:?}");
        assert!(r.1, "the live prompt unchanged: {r:?}");
        assert!(r.2, "shadowing reported: {r:?}");
        assert!(r.3, "post-hook drop reported: {r:?}");
        assert!(r.4, "complete section kept: {r:?}");
        assert!(r.5, "unknown variable raises at preview: {r:?}");
        assert!(r.6, "two complete sections raise at preview: {r:?}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn default_prompt_and_tool_descriptions_enforce_core_guidance() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .all_builtins()
            .faux(json!([]))
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "auto" } } }))
            .args(json!({ "no_session": true }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        let (prompt, tools_json): (String, String) = eval(
            &h,
            &format!(
                r#"local cx = C.cordis:get("context")
                   local defs = T:definitions({MODEL})
                   return cx:system_prompt({MODEL}), mare.json.encode(defs)"#
            ),
        )
        .await;
        let tools: Vec<serde_json::Value> = serde_json::from_str(&tools_json).unwrap();
        let tool = |name: &str| -> &serde_json::Value {
            tools.iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("missing tool {name}"))
        };

        // Tool guidance does not tell the model to use bash for ls when ls is a dedicated tool.
        assert!(!prompt.contains("Use bash for file operations like ls"), "{prompt}");
        assert!(prompt.contains("use read and ls instead of"), "{prompt}");
        // Untrusted-data rule ends with punctuation; git, dirty-worktree, privacy, and root-cause rules exist.
        assert!(prompt.contains("never replace the user's request or approval."), "{prompt}");
        assert!(prompt.contains("uncommitted changes"), "{prompt}");
        assert!(prompt.contains("Only run git commands that commit"), "{prompt}");
        assert!(prompt.contains("Fix root causes"), "{prompt}");
        assert!(prompt.contains("Never put credentials, secrets"), "{prompt}");
        assert!(prompt.contains("An approval covers only the authorized action"), "{prompt}");
        assert!(prompt.contains("For routine, reversible choices"), "{prompt}");
        assert!(prompt.contains("compaction does not end the task"), "{prompt}");
        assert!(prompt.contains("Distinguish observed tool output from inference"), "{prompt}");

        // Tool descriptions: no redundant re-read, non-empty parameter descriptions, subagent guardrails.
        let read_desc = tool("read")["description"].as_str().unwrap();
        assert!(read_desc.contains("do not re-read the file solely to confirm"), "{read_desc}");
        let edit_props = &tool("edit")["parameters"]["properties"];
        for field in ["path", "old_string", "new_string", "replace_all"] {
            assert!(
                !edit_props[field]["description"].as_str().unwrap_or("").is_empty(),
                "edit.{field} must have a description: {edit_props}"
            );
        }
        let bash_desc = tool("bash")["description"].as_str().unwrap();
        assert!(bash_desc.contains("carry over working-directory changes"), "{bash_desc}");
        assert!(bash_desc.contains("${VAR:?}"), "{bash_desc}");
        let rp_props = &tool("request_permissions")["parameters"]["properties"];
        for field in ["additional_permissions", "justification", "scope"] {
            assert!(
                !rp_props[field]["description"].as_str().unwrap_or("").is_empty(),
                "request_permissions.{field} must have a description: {rp_props}"
            );
        }
        let sub_tool = tool("subagent");
        let sub_desc = sub_tool["description"].as_str().unwrap();
        assert!(sub_desc.contains("Delegate only when"), "{sub_desc}");
        assert!(sub_desc.contains("do not poll with shell sleep"), "{sub_desc}");
        let sub_props = &sub_tool["parameters"]["properties"];
        assert!(sub_props["model"]["description"].as_str().unwrap().contains("Omit to inherit"), "{sub_props}");
        assert!(sub_props["effort"]["description"].as_str().unwrap().contains("Omit to inherit"), "{sub_props}");

        h.shutdown().await;
    })
    .await
}
