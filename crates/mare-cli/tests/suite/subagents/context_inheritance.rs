//! Subagent inheritance of parent trust state (project hooks, project skills, project stdio MCP,
//! untrusted AGENTS.md labelling) and parent's live current model/effort.

use super::SUBAGENT_BUILTINS;
use crate::common::scripted::{Part, ScriptedModel, Step, Wire};
use crate::common::*;
use serde_json::{Value, json};

fn write_file(path: &std::path::Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// F1: In a trusted project, a user plugin mirroring the `hooks` plugin (gating project
/// `.mare/hooks.json` on `mare.trusted == true`) must execute its PreToolUse deny guard inside
/// subagents too — preventing the file write on disk and recording that the guard ran.
#[tokio::test]
async fn trusted_project_hooks_guard_subagent_tool_calls() {
    local(async {
        let env = TestEnv::new().await;
        let blocked_file = env.cwd.join("blocked-by-hook.txt");
        let hook_marker = env.cwd.join("hook-ran.marker");

        // Project hook config consumed only when `mare.trusted == true` (mirrors mare-plugins/hooks).
        write_file(
            &env.cwd.join(".mare/hooks.json"),
            &json!({
                "deny_tool": "write",
                "marker": hook_marker.to_string_lossy(),
            })
            .to_string(),
        );
        // User plugin installed globally (mirrors external `hooks` package loaded in parent & child).
        write_file(
            &env.config_dir().join("plugins/project-hooks-guard.lua"),
            r#"
return {
    name = "project-hooks-guard",
    inject = { "policy" },
    apply = function(ctx)
        if mare.trusted ~= true then
            return
        end
        local path = mare.cwd .. "/.mare/hooks.json"
        local raw = mare.fs.read(path)
        if not raw then return end
        local cfg = mare.json.decode(raw)
        ctx.policy:voter("tool/before", function(call)
            if call.name == cfg.deny_tool then
                local f = assert(io.open(cfg.marker, "a"))
                f:write(tostring(mare.mode) .. ":" .. tostring(call.name) .. "\n")
                f:close()
                return { decision = "deny", reason = "blocked by project PreToolUse hook" }
            end
        end)
    end,
}
"#,
        );

        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": blocked_file.to_string_lossy(), "content": "should-not-exist" } }] },
            { "text": "done" }
        ]);
        let mut b = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .user_plugins()
            .faux(script)
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "full-access" } } }))
            .args(json!({ "no_session": true, "trust": "trusted" }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);

        let done: String = eval(
            &h,
            r#"
            local svc = C.cordis:get("subagent")
            local child = svc:start({ prompt = "write blocked file", cwd = mare.cwd })
            local res = svc:wait(child.subagent_id, { timeout_ms = 15000 })
            return mare.json.encode(res)
            "#,
        )
        .await;
        let done: Value = serde_json::from_str(&done).unwrap();
        assert_eq!(done["status"], "completed", "{done}");
        assert!(
            hook_marker.exists(),
            "project PreToolUse hook must execute inside the trusted subagent (marker missing)"
        );
        let marker_text = std::fs::read_to_string(&hook_marker).unwrap();
        assert!(marker_text.contains("child:write"), "marker should record child write denial: {marker_text:?}");
        assert!(
            !blocked_file.exists(),
            "PreToolUse deny hook must prevent the subagent from writing {blocked_file:?} to disk"
        );
        h.shutdown().await;
    })
    .await;
}

/// F4a: In a trusted project, a subagent must see project `.mare/skills/` in its outgoing LLM request.
#[tokio::test]
async fn trusted_project_subagent_inherits_project_skills() {
    local(async {
        let mut env = TestEnv::new().await;
        write_file(
            &env.cwd.join(".mare/skills/proj-only-skill/SKILL.md"),
            "---\nname: proj-only-skill\ndescription: trusted project skill visible to subagents\n---\nbody\n",
        );

        let model = ScriptedModel::start(Wire::Responses, vec![Step::Reply(vec![Part::Text("ok".into())])]).await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        let mut b = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .config(model.host_config())
            .args(json!({ "no_session": true, "trust": "trusted" }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);

        let done: String = eval(
            &h,
            r#"
            local svc = C.cordis:get("subagent")
            local child = svc:start({ prompt = "list skills", cwd = mare.cwd })
            local res = svc:wait(child.subagent_id, { timeout_ms = 15000 })
            return mare.json.encode(res)
            "#,
        )
        .await;
        let done: Value = serde_json::from_str(&done).unwrap();
        assert_eq!(done["status"], "completed", "{done}");

        let reqs = model.requests();
        assert_eq!(reqs.len(), 1, "expected 1 child LLM request, got {}", reqs.len());
        let body = &reqs[0].body;
        assert!(
            body.contains("proj-only-skill") && body.contains("trusted project skill visible to subagents"),
            "trusted subagent request must include project skill: {body}"
        );
        model.finish();
        h.shutdown().await;
    })
    .await;
}

/// F4b: In a trusted project with approved project stdio MCP servers, a subagent must start and
/// invoke the project MCP tool rather than blocking it as `"untrusted"`.
#[tokio::test]
async fn trusted_project_subagent_inherits_project_stdio_mcp() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join(".mare")).unwrap();
        let mcp_log = env.home.path().join("proj-subagent-mcp.log");
        let mcp_cfg = json!({ "log": mcp_log });
        write_file(
            &env.cwd.join(".mcp.json"),
            &json!({
                "mcpServers": {
                    "proj": {
                        "command": fake_mcp_bin(),
                        "args": ["scripted", mcp_cfg.to_string()],
                        "tier": "direct",
                        "required": true
                    }
                }
            })
            .to_string(),
        );

        let script = json!([
            { "tool_calls": [{ "name": "mcp__proj__echo", "arguments": { "text": "hello-from-child" } }] },
            { "text": "done" }
        ]);
        let mut b = env
            .host()
            .builtins(&["subagent", "agents", "mcp"])
            .faux(script)
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "full-access" } } }))
            .args(json!({ "no_session": true, "trust": "trusted", "approve_mcps": true }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);

        wait_until(15, "parent project MCP tool registered", || {
            let hc = h.clone();
            async move { eval::<bool>(&hc, "return T:get('mcp__proj__echo') ~= nil").await }
        })
        .await;

        let raw: String = eval(
            &h,
            r#"
            local svc = C.cordis:get("subagent")
            local child = svc:start({ prompt = "call project mcp", cwd = mare.cwd })
            local res = svc:wait(child.subagent_id, { timeout_ms = 20000 })
            return mare.json.encode(res)
            "#,
        )
        .await;
        let res: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(res["status"], "completed", "{res}");
        let tool_results: Vec<&Value> =
            res["messages"].as_array().into_iter().flatten().filter(|m| m["role"] == "tool_result").collect();
        assert!(!tool_results.is_empty(), "expected at least one tool_result from child MCP call: {res}");
        let first = tool_results[0];
        assert!(
            first["is_error"] != json!(true) && first["content"].to_string().contains("hello-from-child"),
            "subagent must successfully execute project stdio MCP tool (not fail with unknown/untrusted): {first}"
        );
        h.shutdown().await;
    })
    .await;
}

/// F4c: In an untrusted (or undecided) project, a subagent's outgoing request must annotate the
/// project's AGENTS.md with the untrusted project warning — matching the parent.
#[tokio::test]
async fn untrusted_project_subagent_labels_agents_md_as_untrusted() {
    local(async {
        for trust_arg in ["untrusted", "undecided"] {
            let mut env = TestEnv::new().await;
            write_file(&env.cwd.join("AGENTS.md"), "UNTRUSTED-REPO-INSTRUCTION-MARKER");

            let model = ScriptedModel::start(Wire::Responses, vec![Step::Reply(vec![Part::Text("ok".into())])]).await;
            env.set_var("OPENAI_API_KEY", "test-openai");
            let mut b = env
                .host()
                .builtins(SUBAGENT_BUILTINS)
                .config(model.host_config())
                .args(json!({ "no_session": true, "trust": trust_arg }));
            b.opts.project_trusted = false;
            let h = b.build().await;
            expose(&h);

            let done: String = eval(
                &h,
                r#"
                local svc = C.cordis:get("subagent")
                local child = svc:start({ prompt = "check agents md", cwd = mare.cwd })
                local res = svc:wait(child.subagent_id, { timeout_ms = 15000 })
                return mare.json.encode(res)
                "#,
            )
            .await;
            let done: Value = serde_json::from_str(&done).unwrap();
            assert_eq!(done["status"], "completed", "{trust_arg}: {done}");

            let reqs = model.requests();
            assert_eq!(reqs.len(), 1, "{trust_arg}: expected 1 child request");
            let body = &reqs[0].body;
            assert!(
                body.contains("UNTRUSTED-REPO-INSTRUCTION-MARKER"),
                "{trust_arg}: child request should include AGENTS.md: {body}"
            );
            assert!(
                body.contains("from an untrusted project"),
                "{trust_arg}: child AGENTS.md must carry the untrusted project annotation: {body}"
            );
            model.finish();
            h.shutdown().await;
        }
    })
    .await;
}

/// F6: When the parent switches its model (and effort) mid-session via `llm:set_model` /
/// `llm:set_effort`, a subagent started without an explicit `model` must send its outgoing request
/// using the parent's current live model and effort — not the stale startup config model.
#[tokio::test]
async fn subagent_without_explicit_model_inherits_parent_current_model_and_effort() {
    local(async {
        let mut env = TestEnv::new().await;
        let model = ScriptedModel::start(
            Wire::Responses,
            vec![
                Step::Reply(vec![Part::Text("inherited-ok".into())]),
                Step::Reply(vec![Part::Text("override-ok".into())]),
            ],
        )
        .await;
        env.set_var("OPENAI_API_KEY", "test-openai");

        // Startup model is `openai/gpt-6.1-terra`.
        let mut b = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .config(model.host_config())
            .args(json!({ "no_session": true, "trust": "trusted" }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);

        // Switch parent's live model mid-session to `openai/gpt-6.1-sol` + effort `high` (session-only).
        let (done_inherited, done_override): (String, String) = eval(
            &h,
            r#"
            local llm, svc = C.cordis:get("llm"), C.cordis:get("subagent")
            llm:set_model("openai/gpt-6.1-sol", { target = "session", save = false })
            llm:set_effort("high", { target = "session", save = false })

            local c1 = svc:start({ prompt = "inherit live model", cwd = mare.cwd })
            local r1 = svc:wait(c1.subagent_id, { timeout_ms = 15000 })

            local c2 = svc:start({ prompt = "explicit override", cwd = mare.cwd, model = "openai/gpt-6-luna" })
            local r2 = svc:wait(c2.subagent_id, { timeout_ms = 15000 })

            return mare.json.encode(r1), mare.json.encode(r2)
            "#,
        )
        .await;
        let r1: Value = serde_json::from_str(&done_inherited).unwrap();
        let r2: Value = serde_json::from_str(&done_override).unwrap();
        assert_eq!(r1["status"], "completed", "{r1}");
        assert_eq!(r2["status"], "completed", "{r2}");

        let reqs = model.requests();
        assert_eq!(reqs.len(), 2, "expected 2 child requests, got {}", reqs.len());
        let b0: Value = serde_json::from_str(&reqs[0].body).unwrap();
        let b1: Value = serde_json::from_str(&reqs[1].body).unwrap();
        assert_eq!(
            b0["model"], "gpt-6.1-sol",
            "subagent without explicit model must inherit parent's current live model, not startup config: {b0}"
        );
        assert_eq!(
            b0["reasoning"]["effort"], "high",
            "subagent inheriting parent's live model must also inherit parent's current effort: {b0}"
        );
        assert_eq!(
            b1["model"], "gpt-6-luna",
            "explicit model override must still win over parent's current model: {b1}"
        );
        model.finish();
        h.shutdown().await;
    })
    .await;
}

/// Security boundary: a trusted parent spawning a subagent with `cwd` in a DIFFERENT untrusted
/// workspace must NOT propagate `project_trusted = true` to that foreign directory.
#[tokio::test]
async fn trusted_parent_does_not_trust_foreign_untrusted_subagent_cwd() {
    local(async {
        let mut env = TestEnv::new().await;
        let foreign = tempfile::tempdir().unwrap();
        write_file(
            &foreign.path().join(".mare/skills/foreign-skill/SKILL.md"),
            "---\nname: foreign-skill\ndescription: untrusted foreign skill\n---\nbody\n",
        );
        write_file(&foreign.path().join("AGENTS.md"), "FOREIGN-UNTRUSTED-DOC");

        let model = ScriptedModel::start(Wire::Responses, vec![Step::Reply(vec![Part::Text("ok".into())])]).await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        let mut b = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .config(model.host_config())
            .args(json!({ "no_session": true, "trust": "trusted" }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "FOREIGN_CWD", foreign.path().to_string_lossy().to_string());

        let done: String = eval(
            &h,
            r#"
            local svc = C.cordis:get("subagent")
            local child = svc:start({ prompt = "inspect foreign dir", cwd = FOREIGN_CWD })
            local res = svc:wait(child.subagent_id, { timeout_ms = 15000 })
            return mare.json.encode(res)
            "#,
        )
        .await;
        let done: Value = serde_json::from_str(&done).unwrap();
        assert_eq!(done["status"], "completed", "{done}");

        let reqs = model.requests();
        assert_eq!(reqs.len(), 1);
        let body = &reqs[0].body;
        assert!(!body.contains("foreign-skill"), "untrusted foreign directory's skills must NOT load: {body}");
        assert!(body.contains("FOREIGN-UNTRUSTED-DOC"), "foreign AGENTS.md should load: {body}");
        assert!(
            body.contains("from an untrusted project"),
            "foreign untrusted directory's AGENTS.md must carry untrusted annotation: {body}"
        );
        model.finish();
        h.shutdown().await;
    })
    .await;
}

/// Security boundary: CLI `--no-trust` (`trust_source = "cli"`, `project_trusted = false`) must
/// override saved trust for a foreign `cwd` across both direct subagents and grandchildren —
/// blocking project hooks, project stdio MCP servers, and project skills.
#[tokio::test]
async fn no_trust_blocks_saved_trusted_foreign_cwd_subagent_hooks_mcp_and_skills() {
    local(async {
        let mut env = TestEnv::new().await;
        let foreign = tempfile::tempdir().unwrap();
        let foreign_cwd = dunce::canonicalize(foreign.path()).unwrap();
        // Save the foreign directory as trusted in trust.json; `--no-trust` must still win.
        mare_config::trust::set(&foreign_cwd, true).unwrap();

        let hook_marker = env.home.path().join("no-trust-foreign-hook.marker");
        let mcp_log = env.home.path().join("no-trust-foreign-mcp.log");
        write_file(
            &foreign_cwd.join(".mare/hooks.json"),
            &json!({ "marker": hook_marker.to_string_lossy() }).to_string(),
        );
        write_file(
            &env.config_dir().join("plugins/foreign-hooks-probe.lua"),
            r#"
return {
    name = "foreign-hooks-probe",
    apply = function()
        if mare.trusted ~= true then return end
        local raw = mare.fs.read(mare.cwd .. "/.mare/hooks.json")
        if not raw then return end
        local cfg = mare.json.decode(raw)
        local f = assert(io.open(cfg.marker, "a"))
        f:write(tostring(mare.mode) .. "\n")
        f:close()
    end,
}
"#,
        );
        write_file(
            &foreign_cwd.join(".mare/skills/foreign-saved-skill/SKILL.md"),
            "---\nname: foreign-saved-skill\ndescription: saved-trusted foreign skill\n---\nbody\n",
        );
        write_file(
            &foreign_cwd.join(".mcp.json"),
            &json!({
                "mcpServers": {
                    "proj": {
                        "command": fake_mcp_bin(),
                        "args": ["scripted", json!({ "log": mcp_log }).to_string()],
                        "tier": "direct"
                    }
                }
            })
            .to_string(),
        );
        // Pre-approve the foreign project MCP server once in a trusted host so that only
        // `--no-trust` stands between the foreign subagent and launching `proj`.
        {
            let mut approve_b = env
                .host()
                .builtins(&["mcp"])
                .args(json!({ "no_session": true, "trust": "trusted", "approve_mcps": true }));
            approve_b.opts.cwd = foreign_cwd.clone();
            approve_b.opts.project_trusted = true;
            let approve_h = approve_b.build().await;
            expose(&approve_h);
            wait_until(15, "foreign MCP server pre-approved", || {
                let hc = approve_h.clone();
                async move { eval::<bool>(&hc, "return T:get('mcp__proj__echo') ~= nil").await }
            })
            .await;
            approve_h.shutdown().await;
            let _ = std::fs::remove_file(&mcp_log);
        }
        write_file(&foreign_cwd.join("AGENTS.md"), "FOREIGN-NO-TRUST-DOC");

        let foreign_str = foreign_cwd.to_string_lossy().to_string();
        let model = ScriptedModel::start(
            Wire::Responses,
            vec![
                Step::Reply(vec![Part::ToolCall {
                    id: "call_nested".into(),
                    name: "subagent".into(),
                    args: json!({
                        "prompt": "grandchild inspect foreign dir",
                        "cwd": foreign_str,
                        "wait": true
                    }),
                }]),
                Step::Reply(vec![Part::Text("grandchild-ok".into())]),
                Step::Reply(vec![Part::Text("child-ok".into())]),
            ],
        )
        .await;
        env.set_var("OPENAI_API_KEY", "test-openai");

        let mut cfg = model.host_config();
        cfg["agents"] = json!({ "max_depth": 2 });
        let mut b = env.host().builtins(&["subagent", "agents", "mcp"]).user_plugins().config(cfg).args(json!({
            "no_session": true,
            "trust": "untrusted",
            "trust_source": "cli",
            "approve_mcps": true
        }));
        b.opts.project_trusted = false;
        let h = b.build().await;
        expose(&h);
        global(&h, "FOREIGN_CWD", foreign_cwd.to_string_lossy().to_string());

        let done: String = eval(
            &h,
            r#"
            local svc = C.cordis:get("subagent")
            local child = svc:start({ prompt = "inspect foreign dir under --no-trust", cwd = FOREIGN_CWD })
            local res = svc:wait(child.subagent_id, { timeout_ms = 20000 })
            return mare.json.encode(res)
            "#,
        )
        .await;
        let done: Value = serde_json::from_str(&done).unwrap();
        assert_eq!(done["status"], "completed", "{done}");

        assert!(
            !hook_marker.exists(),
            "--no-trust must prevent project hooks in saved-trusted foreign subagent/grandchild"
        );
        assert!(
            !mcp_log.exists(),
            "--no-trust must prevent project stdio MCP startup in saved-trusted foreign subagent/grandchild"
        );

        let reqs = model.requests();
        assert_eq!(reqs.len(), 3, "expected child(2) + grandchild(1) requests, got {}", reqs.len());
        for (idx, req) in reqs.iter().enumerate() {
            assert!(
                !req.body.contains("foreign-saved-skill"),
                "request {idx}: --no-trust must block foreign project skills: {}",
                req.body
            );
            assert!(
                !req.body.contains("mcp__proj__echo"),
                "request {idx}: --no-trust must block foreign project MCP tools: {}",
                req.body
            );
            assert!(
                req.body.contains("FOREIGN-NO-TRUST-DOC") && req.body.contains("from an untrusted project"),
                "request {idx}: foreign AGENTS.md must stay annotated as untrusted under --no-trust: {}",
                req.body
            );
        }
        model.finish();
        h.shutdown().await;
    })
    .await;
}

/// Issue #115: empty-string optional `type`, `model`, `effort`, and `system` overrides passed to
/// the model-callable `subagent` tool must behave identically to omitting those fields (preserving
/// parent model/effort inheritance and type defaults such as `explore`'s system instruction),
/// while non-empty overrides continue to apply.
#[tokio::test]
async fn subagent_tool_empty_string_overrides_match_omitted_and_keep_non_empty_overrides() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-openai");

        let model = ScriptedModel::start(
            Wire::Responses,
            vec![
                // Parent turn 1: omitted optional fields
                Step::Reply(vec![Part::ToolCall {
                    id: "c_omitted".into(),
                    name: "subagent".into(),
                    args: json!({ "prompt": "check defaults", "wait": true }),
                }]),
                Step::Reply(vec![Part::Text("child-omitted-ok".into())]),
                Step::Reply(vec![Part::Text("parent-1-done".into())]),
                // Parent turn 2: all four optional fields passed as ""
                Step::Reply(vec![Part::ToolCall {
                    id: "c_empty".into(),
                    name: "subagent".into(),
                    args: json!({
                        "prompt": "check defaults",
                        "type": "",
                        "model": "",
                        "effort": "",
                        "system": "",
                        "wait": true
                    }),
                }]),
                Step::Reply(vec![Part::Text("child-empty-ok".into())]),
                Step::Reply(vec![Part::Text("parent-2-done".into())]),
                // Parent turn 3: `type = "explore"` with empty model/effort/system -> keeps explore system prompt
                Step::Reply(vec![Part::ToolCall {
                    id: "c_explore_empty".into(),
                    name: "subagent".into(),
                    args: json!({
                        "prompt": "inspect code",
                        "type": "explore",
                        "model": "",
                        "effort": "",
                        "system": "",
                        "wait": true
                    }),
                }]),
                Step::Reply(vec![Part::Text("child-explore-ok".into())]),
                Step::Reply(vec![Part::Text("parent-3-done".into())]),
                // Parent turn 4: non-empty model/effort/system overrides still apply
                Step::Reply(vec![Part::ToolCall {
                    id: "c_override".into(),
                    name: "subagent".into(),
                    args: json!({
                        "prompt": "inspect with override",
                        "type": "explore",
                        "model": "openai/gpt-6-luna",
                        "effort": "low",
                        "system": "CUSTOM_SUBAGENT_SYSTEM_OVERRIDE",
                        "wait": true
                    }),
                }]),
                Step::Reply(vec![Part::Text("child-override-ok".into())]),
                Step::Reply(vec![Part::Text("parent-4-done".into())]),
            ],
        )
        .await;

        let mut b = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .config(model.host_config())
            .args(json!({ "no_session": true, "trust": "trusted" }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);

        // Switch parent live model and effort so inheritance is distinguishable from startup defaults.
        exec(
            &h,
            r#"
            local llm = C.cordis:get("llm")
            llm:set_model("openai/gpt-6.1-sol", { target = "session", save = false })
            llm:set_effort("high", { target = "session", save = false })
            "#,
        )
        .await;

        for (i, prompt) in ["run 1", "run 2", "run 3", "run 4"].iter().enumerate() {
            let res = h.prompt(prompt, json!({})).await.unwrap();
            assert_eq!(res["text"], format!("parent-{}-done", i + 1), "{res}");
        }

        let reqs = model.requests();
        // Each turn makes 3 requests: parent step 1 -> child -> parent step 2.
        assert_eq!(reqs.len(), 12, "expected 12 total requests (4 parent turns * 3), got {}", reqs.len());

        let child_omitted: Value = serde_json::from_str(&reqs[1].body).unwrap();
        let child_empty: Value = serde_json::from_str(&reqs[4].body).unwrap();
        let child_explore_empty: Value = serde_json::from_str(&reqs[7].body).unwrap();
        let child_override: Value = serde_json::from_str(&reqs[10].body).unwrap();

        assert_eq!(child_omitted["model"], "gpt-6.1-sol", "{child_omitted}");
        assert_eq!(child_omitted["reasoning"]["effort"], "high", "{child_omitted}");

        // Omitted and empty-string fields must produce identical child model, effort, and system prompt.
        assert_eq!(
            child_empty["model"], child_omitted["model"],
            "empty model must inherit like omitted: {child_empty}"
        );
        assert_eq!(
            child_empty["reasoning"]["effort"], child_omitted["reasoning"]["effort"],
            "empty effort must inherit like omitted: {child_empty}"
        );
        assert_eq!(
            child_empty["instructions"], child_omitted["instructions"],
            "empty type/system must inherit the exact same instructions as omitted"
        );

        // `type = "explore"` with empty overrides preserves explore's system instruction and parent model/effort.
        assert_eq!(child_explore_empty["model"], "gpt-6.1-sol", "{child_explore_empty}");
        assert_eq!(child_explore_empty["reasoning"]["effort"], "high", "{child_explore_empty}");
        assert!(
            child_explore_empty["instructions"]
                .as_str()
                .is_some_and(|s| s.contains("Inspect the repository without modifying it")),
            "empty system must not skip explore's default system instruction: {child_explore_empty}"
        );

        // Non-empty overrides still apply cleanly.
        assert_eq!(child_override["model"], "gpt-6-luna", "{child_override}");
        assert_eq!(child_override["reasoning"]["effort"], "low", "{child_override}");
        assert!(
            child_override["instructions"].as_str().is_some_and(|s| s.contains("CUSTOM_SUBAGENT_SYSTEM_OVERRIDE")),
            "non-empty system override must apply: {child_override}"
        );

        model.finish();
        h.shutdown().await;
    })
    .await;
}
