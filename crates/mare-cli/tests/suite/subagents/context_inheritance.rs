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
