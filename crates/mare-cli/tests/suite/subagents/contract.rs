//! Guard the built-in subagent contract against drift from its provider and verify contract behavior.

use super::SUBAGENT_BUILTINS;
use crate::common::*;
use serde_json::{Value, json};

#[tokio::test]
async fn subagent_type_registry_has_one_owned_and_isolated_view() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(SUBAGENT_BUILTINS).faux(json!([])).no_session().build().await;
        expose(&h);
        let ok: bool = eval(
            &h,
            r#"
            local svc = C.cordis:get("subagent")
            local initial = svc:list_types().data
            assert(initial[1] and initial[1].tools and initial[1].tools[1], "fixture needs a tool list")
            local original = initial[1].tools[1]
            initial[1].tools[1] = "changed"
            assert(svc:list_types().data[1].tools[1] == original, "list must not expose the registry")

            local missing, missing_err = pcall(function() svc:start({prompt = "x", type = "missing"}) end)
            assert(not missing and missing_err.code == "subagent.unknown_type", tostring(missing_err))
            local invalid, invalid_err = pcall(function()
              svc:register_type({name = "../bad", description = "bad"})
            end)
            assert(not invalid and invalid_err.code == "invalid_arguments", tostring(invalid_err))
            local first = svc:register_type({name = "worker", description = "first", tools = {"read"}})
            local dup, dup_err = pcall(function()
              svc:register_type({name = "worker", description = "duplicate"})
            end)
            assert(not dup and dup_err.code == "subagent.duplicate_type", tostring(dup_err))
            local second = svc:register_type({name = "worker", description = "second"}, {replace = true})
            local function worker() for _, v in ipairs(svc:list_types().data) do if v.name == "worker" then return v end end end
            assert(worker().description == "second")
            first()
            assert(worker().description == "second", "old disposer must not erase a replacement")
            second()
            assert(worker() == nil)
            first = svc:register_type({name = "worker", description = "previous"})
            second = svc:register_type({name = "worker", description = "replacement"}, {replace = true})
            second()
            assert(worker().description == "previous", "releasing the replacement must restore its live predecessor")
            first()
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn type_registrations_do_not_cross_same_host_session_replacement() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(SUBAGENT_BUILTINS).faux(json!([])).build().await;
        expose(&h);
        let (leaked, registered, retained): (bool, bool, bool) = eval(
            &h,
            r#"
          S:create()
          local svc = C.cordis:get("subagent")
          local old = svc:register_type({name="session-only", description="old session"})
          S:create()
          local function find()
            for _, def in ipairs(svc:list_types().data) do
              if def.name == "session-only" then return def end
            end
          end
          local leaked = find() ~= nil
          local ok, new = pcall(svc.register_type, svc, {name="session-only", description="new session"})
          old()
          local retained = find() and find().description == "new session" or false
          if ok then new() end
          return leaked, ok, retained
        "#,
        )
        .await;
        assert!(!leaked && registered && retained, "leaked={leaked}, registered={registered}, retained={retained}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn startup_type_registrations_follow_an_explicit_session_id() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("register-startup-type.lua"),
            r#"
          return {name="register-startup-type", inject={"subagent"}, apply=function(ctx)
            ctx.subagent:register_type({name="startup-type", description="registered before session startup"})
          end}
        "#,
        )
        .unwrap();
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .user_plugins()
            .faux(json!([]))
            .args(json!({"session_id":"explicit-session-id"}))
            .build()
            .await;
        expose(&h);
        let (session, found, next_session, still_found): (String, bool, String, bool) = eval(
            &h,
            r#"
          local found = false
          for _, def in ipairs(C.cordis:get("subagent"):list_types().data) do
            if def.name == "startup-type" then found = true end
          end
          local id = S:info().session_id
          S:create()
          local after = false
          for _, def in ipairs(C.cordis:get("subagent"):list_types().data) do
            if def.name == "startup-type" then after = true end
          end
          return id, found, S:info().session_id, after
        "#,
        )
        .await;
        assert_eq!(session, "explicit-session-id");
        assert!(found, "the initial session's explicit identity must retain startup registrations");
        assert_ne!(session, next_session);
        assert!(still_found, "plugin apply registrations must survive a same-host session switch");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn dynamic_type_after_reload_stays_scoped_to_the_current_session() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(SUBAGENT_BUILTINS).faux(json!([{ "text":"done" }])).build().await;
        expose(&h);
        h.prompt("persist before reload", json!({})).await.unwrap();
        exec(&h, "mare.reload(); C.cordis:get('kernel'):reload_barrier()").await;
        expose(&h);
        let (before, after): (bool, bool) = eval(
            &h,
            r#"
          local svc = C.cordis:get("subagent")
          local function has()
            for _, def in ipairs(svc:list_types().data) do if def.name == "after-reload" then return true end end
            return false
          end
          local release = svc:register_type({name="after-reload", description="session-scoped"})
          local before = has()
          S:create()
          local after = has()
          release()
          return before, after
        "#,
        )
        .await;
        assert!(before && !after, "dynamic registration crossed a session after reload: {before}/{after}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn late_plugin_apply_types_follow_their_owner_but_dynamic_types_follow_the_session() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(SUBAGENT_BUILTINS).faux(json!([])).build().await;
        expose(&h);
        let (before, after, dynamic_gone, disposed, override_restored): (bool, bool, bool, bool, bool) = eval(&h, r#"
          local tx, rx = mare.task.channel()
          local plugin_ctx
          local fiber = C.cordis:plugin({name="late-type-owner", inject={"subagent"}, apply=function(ctx)
            plugin_ctx = ctx
            ctx.subagent:register_type({name="late-default", description="plugin default"})
            ctx.subagent:register_type({name="explore", description="plugin override"}, {replace=true})
            tx:send(true)
          end})
          assert(mare.task.timeout(5000, function() return rx:recv() end), "late plugin did not load")
          local svc = C.cordis:get("subagent")
          local function has(name)
            for _, def in ipairs(svc:list_types().data) do if def.name == name then return true end end
            return false
          end
          local function description(name)
            for _, def in ipairs(svc:list_types().data) do if def.name == name then return def.description end end
          end
          local before = has("late-default")
          S:create()
          local after = has("late-default")
          plugin_ctx.subagent:register_type({name="session-dynamic", description="only this session"})
          assert(has("session-dynamic"))
          S:create()
          local dynamic_gone = not has("session-dynamic") and has("late-default") and description("explore") == "plugin override"
          local done = fiber:dispose()
          if done then require("cordis").await(done) end
          return before, after, dynamic_gone, not has("late-default"), description("explore") == "Inspect code and report findings."
        "#).await;
        assert!(before && after && dynamic_gone && disposed, "late apply={before}/{after}, session scope={dynamic_gone}, owner disposal={disposed}");
        assert!(override_restored, "disposing the plugin must restore the built-in type");
        h.shutdown().await;
    }).await;
}

#[tokio::test]
async fn absent_project_agents_directory_does_not_report_an_escape() {
    local(async {
        let env = TestEnv::new().await;
        assert!(!env.cwd.join(".mare/agents").exists());
        let h = env.host().builtins(SUBAGENT_BUILTINS).project_trusted(true).faux(json!([])).build().await;
        expose(&h);
        exec(
            &h,
            r#"
          OLD_LOG, WARNINGS = mare.log, {}
          mare.log = function(level, text)
            if level == "warn" then WARNINGS[#WARNINGS+1] = tostring(text) end
          end
          mare.trusted = true
        "#,
        )
        .await;
        h.kernel.emit_json("trust/changed", json!({ "trusted": true })).await;
        let warnings: String = eval(&h, r#"mare.log = OLD_LOG; return table.concat(WARNINGS, "\n")"#).await;
        assert!(!warnings.contains("directory escapes"), "nonexistent project directory is not an escape: {warnings}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn discovered_types_follow_trust_and_preserve_owned_overrides() {
    local(async {
        let env = TestEnv::new().await;
        let user = env.config_dir().join("agents");
        let project = env.cwd.join(".mare/agents");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(user.join("shared.md"), "---\ndescription: user layer\n---\nuser body").unwrap();
        std::fs::write(project.join("shared.md"), "---\ndescription: project layer\n---\nproject body").unwrap();
        let h = env.host().builtins(SUBAGENT_BUILTINS).project_trusted(true).faux(json!([])).build().await;
        expose(&h);
        let k_emit = h.kernel.clone();
        let emit_trust = h
            .lua()
            .create_async_function(move |_, trusted: bool| {
                let k = k_emit.clone();
                async move {
                    k.emit_json("trust/changed", json!({ "trusted": trusted })).await;
                    Ok(())
                }
            })
            .unwrap();
        h.lua().globals().set("_emit_trust", emit_trust).unwrap();
        let (initial, revoked, restored, overridden, disposed, generations): (String, String, String, String, String, bool) = eval(&h, r#"
          local svc = C.cordis:get("subagent")
          local function description()
            for _, def in ipairs(svc:list_types().data) do
              if def.name == "shared" then return def.description end
            end
            return "missing"
          end
          local initial = description()
          local before = T:revision()
          mare.trusted = false
          _emit_trust(false)
          local revoked, after_revoked = description(), T:revision()
          mare.trusted = true
          _emit_trust(true)
          local restored, after_restored = description(), T:revision()
          local remove = svc:register_type({name="shared", description="owned override"}, {replace=true})
          mare.trusted = false
          _emit_trust(false)
          local overridden = description()
          remove()
          return initial, revoked, restored, overridden, description(), after_revoked == before + 1 and after_restored == after_revoked + 1
        "#).await;
        assert_eq!((initial.as_str(), revoked.as_str(), restored.as_str()), ("project layer", "user layer", "project layer"));
        assert_eq!((overridden.as_str(), disposed.as_str()), ("owned override", "user layer"));
        assert!(generations, "each discovered catalog change must invalidate once");
        h.shutdown().await;
    }).await;
}

#[tokio::test]
async fn subagent_tool_schema_tracks_the_live_type_registry() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(SUBAGENT_BUILTINS).faux(json!([])).no_session().build().await;
        expose(&h);
        let ok: bool = eval(
            &h,
            r#"
            local svc = C.cordis:get("subagent")
            local function candidates()
              for _, def in ipairs(T:definitions()) do
                if def.name == "subagent" then return def.parameters.properties.type.enum end
              end
              error("subagent declaration absent")
            end
            local initial = candidates()
            assert(table.concat(initial, ",") == "explore,general,plan")
            local before = T:revision()
            local release = svc:register_type({name = "writer", description = "Write a change."})
            assert(T:revision() == before + 1, "one type registration invalidates tool definitions once")
            assert(table.concat(candidates(), ",") == "explore,general,plan,writer")
            assert(table.concat(initial, ",") == "explore,general,plan", "old declarations stay detached")
            release()
            assert(T:revision() == before + 2)
            assert(table.concat(candidates(), ",") == "explore,general,plan")
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn subagent_model_catalog_changes_disclose_once_without_changing_tool_schema() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(SUBAGENT_BUILTINS).faux(json!([{ "text": "done" }])).build().await;
        expose(&h);
        let (first, unchanged, changed, texts, schemas, generations): (usize, usize, usize, String, bool, bool) = eval(
            &h,
            r#"
          local llm = C.cordis:get("llm")
          local original = llm.list_models
          local refs = {"catalog/first"}
          llm.list_models = function()
            local out = mare.json.array()
            for _, ref in ipairs(refs) do out[#out + 1] = {ref=ref, available=true, ready=true} end
            return {data=out}
          end
          local function schema()
            for _, def in ipairs(T:definitions()) do
              if def.name == "subagent" then return mare.json.encode(def.parameters) end
            end
            error("missing subagent tool")
          end
          local function disclosures()
            local values = {}
            for _, entry in ipairs(S:entries()) do
              local msg = entry.type == "message" and entry.message
              if msg and msg.context_disclosure and msg.context_disclosure.namespace == "subagent.models" then
                values[#values + 1] = msg.content[1].text
              end
            end
            return values
          end
          local before, generation = schema(), T:revision()
          A:wait(A:prompt("first").turn_id)
          local first = #disclosures()
          A:wait(A:prompt("unchanged").turn_id)
          local unchanged = #disclosures()
          refs = {"catalog/first", "catalog/second"}
          A:wait(A:prompt("changed").turn_id)
          local list = disclosures()
          llm.list_models = original
          return first, unchanged, #list, table.concat(list, "\n"), before == schema(), generation == T:revision()
        "#,
        )
        .await;
        assert_eq!((first, unchanged, changed), (1, 1, 2), "{texts}");
        assert!(
            texts.contains("<system-reminder>") && texts.contains("catalog/first") && texts.contains("catalog/second"),
            "{texts}"
        );
        assert!(schemas && generations, "model changes must not invalidate the tool schema");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn subagent_model_catalog_discloses_on_real_session_path_with_auth_file() {
    local(async {
        let env = TestEnv::new().await;
        // Use all built-in plugins (real providers and subagent) without stubbing
        // llm.list_models so readiness exercises auth_store -> mare.fs.read.
        let h = env.host().all_builtins().faux(json!([{ "text": "done" }])).build_exposed().await;
        let result_json: String = eval(
            &h,
            r#"
            local llm = C.cordis:get("llm")
            llm:auth_set("opencode-go", { api_key = "test-opencode-key" })
            local seen_in_request = mare.json.array()
            C.cordis:on("request/build", function(req, next)
                for _, m in ipairs(req.messages or {}) do
                    if m.context_disclosure and m.context_disclosure.namespace == "subagent.models" then
                        seen_in_request[#seen_in_request + 1] = m.content[1].text
                    end
                end
                return next()
            end)
            local res = A:wait(A:prompt("list models").turn_id)
            local persisted = mare.json.array()
            for _, entry in ipairs(S:entries()) do
                local msg = entry.type == "message" and entry.message
                if msg and msg.context_disclosure and msg.context_disclosure.namespace == "subagent.models" then
                    persisted[#persisted + 1] = msg.content[1].text
                end
            end
            return mare.json.encode({
                text = res.text,
                err = res.error,
                seen = seen_in_request,
                persisted = persisted,
            })
        "#,
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert_eq!(r["text"], "done", "{r}");
        assert!(r["err"].is_null(), "{r}");
        let seen = r["seen"].as_array().unwrap();
        assert_eq!(seen.len(), 1, "expected real subagent.models disclosure in request/build: {r}");
        let text = seen[0].as_str().unwrap_or("");
        assert!(
            text.contains("<system-reminder>Available subagent models:") && text.contains("opencode-go/"),
            "expected auth.json-backed opencode-go models in disclosure: {r}"
        );
        assert_eq!(r["persisted"].as_array().unwrap().len(), 1, "disclosure must persist to session: {r}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn model_catalog_is_not_disclosed_when_the_subagent_tool_is_excluded() {
    for cli in [true, false] {
        local(async {
            let env = TestEnv::new().await;
            let args = if cli { json!({"agent":"explore"}) } else { json!({}) };
            let h = env
                .host()
                .builtins(SUBAGENT_BUILTINS)
                .faux(json!([{ "text":"prime" }, { "text":"done" }]))
                .args(args)
                .build()
                .await;
            expose(&h);
            exec(
                &h,
                r#"
              C.cordis:get("llm").list_models = function()
                return {data=mare.json.array({{ref="known/model", available=true, ready=true}})}
              end
            "#,
            )
            .await;
            let opts = if cli { json!({}) } else { json!({"tools":["read"]}) };
            let first = h.prompt("prime", opts.clone()).await.unwrap();
            assert_eq!(first["text"], "prime", "{first}");
            let response = h.prompt("inspect", opts).await.unwrap();
            assert_eq!(response["text"], "done", "{response}");
            let history: String = eval(&h, "return mare.json.encode(A:messages())").await;
            assert!(history.contains("inspect"), "prompt missing: {history}");
            assert!(!history.contains("Available subagent models"), "excluded tool disclosed a catalog: {history}");
            h.shutdown().await;
        })
        .await;
    }
}

#[tokio::test]
async fn model_catalog_is_not_disclosed_when_llm_constraint_excludes_subagent() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([{ "text":"done" }]))
            .config(json!({ "plugins": { "preset": "minimal", "subagent": { "enabled": true } } }))
            .build_exposed()
            .await;
        exec(
            &h,
            r#"
            local llm = C.cordis:get("llm")
            assert(T:get("subagent"), "the tool must be present before applying the constraint")
            llm.list_models = function()
                return {data=mare.json.array({{ref="known/model", available=true, ready=true}})}
            end
            llm:constrain_tools(function() return {"read"} end)
        "#,
        )
        .await;
        let response = h.prompt("inspect", json!({})).await.unwrap();
        assert_eq!(response["text"], "done", "{response}");
        let history: String = eval(&h, "return mare.json.encode(A:messages())").await;
        assert!(history.contains("inspect"), "prompt missing: {history}");
        assert!(!history.contains("Available subagent models"), "excluded tool disclosed a catalog: {history}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn agent_files_obey_layer_precedence_trust_and_frontmatter_validation() {
    local(async {
        let env = TestEnv::new().await;
        let user = env.config_dir().join("agents");
        let project = env.cwd.join(".mare/agents");
        let extra1 = env.home.path().join("extra-one");
        let extra2 = env.home.path().join("extra-two");
        for dir in [&user, &project, &extra1, &extra2] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(user.join("a.md"), "---\nname: shared\ndescription: user-first\n---\nuser body").unwrap();
        std::fs::write(user.join("z.md"), "---\nname: shared\ndescription: user-later\n---\nignored").unwrap();
        std::fs::write(
            user.join("empty.md"),
            "\u{feff}---\r\ndescription: Empty tools\r\ntools: []\r\n---\r\nbody\r\n",
        )
        .unwrap();
        std::fs::write(user.join("bad.md"), "---\ndescription: [unterminated\n---\nbad").unwrap();
        std::fs::create_dir_all(user.join("nested")).unwrap();
        std::fs::write(user.join("nested/hidden.md"), "---\ndescription: hidden\n---\nhidden").unwrap();
        std::fs::write(extra1.join("shared.md"), "---\ndescription: extra-one\n---\none").unwrap();
        std::fs::write(extra2.join("shared.md"), "---\ndescription: extra-two\n---\ntwo").unwrap();
        std::fs::write(project.join("shared.md"), "---\ndescription: project\n---\nproject body").unwrap();
        #[cfg(unix)]
        {
            let outside = env.home.path().join("outside.md");
            std::fs::write(&outside, "---\nname: escape\ndescription: outside\n---\nsecret").unwrap();
            std::os::unix::fs::symlink(outside, project.join("escape.md")).unwrap();
        }
        for trusted in [false, true] {
            let h = env
                .host()
                .builtins(SUBAGENT_BUILTINS)
                .user_plugins()
                .project_trusted(trusted)
                .faux(json!([]))
                .config(json!({ "plugins": { "subagent": { "extra_agent_dirs": [extra1, extra2] } } }))
                .no_session()
                .build()
                .await;
            expose(&h);
            let encoded: String = eval(&h, "return mare.json.encode(C.cordis:get('subagent'):list_types().data)").await;
            let types: Value = serde_json::from_str(&encoded).unwrap();
            let list = types.as_array().unwrap();
            let get = |name: &str| list.iter().find(|v| v["name"] == name);
            assert_eq!(get("shared").unwrap()["description"], if trusted { "project" } else { "extra-two" });
            assert_eq!(get("empty").unwrap()["system"], "body\n");
            assert_eq!(get("empty").unwrap()["tools"], json!([]));
            assert!(get("bad").is_none() && get("hidden").is_none() && get("escape").is_none());
            h.shutdown().await;
        }
    })
    .await;
}

#[test]
fn cli_exposes_an_agent_type_option() {
    let output = mare_cmd().arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--agent <NAME>"), "agent type option is absent: {help}");
}

#[tokio::test]
async fn main_agent_uses_the_same_discovered_type_as_subagents() {
    local(async {
        let env = TestEnv::new().await;
        let agents = env.config_dir().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("research.md"),
            "---\ndescription: Research type\ntools: [read]\n---\nSYSTEM-FROM-REGISTRY",
        )
        .unwrap();
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .user_plugins()
            .faux(json!([{ "text": "done" }]))
            .args(json!({ "agent": "research", "no_session": true }))
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"
          C.cordis:on("request/build", function(req, next)
            CAPTURED = mare.json.encode({system = req.system, tools = req.tools})
            return next()
          end)
        "#,
        )
        .await;
        let response = h.prompt("inspect", json!({})).await.unwrap();
        assert!(response["error"].is_null(), "{response}");
        let captured: String = eval(&h, "return CAPTURED").await;
        let captured: Value = serde_json::from_str(&captured).unwrap();
        assert_eq!(captured["system"], "SYSTEM-FROM-REGISTRY");
        let names: Vec<_> = captured["tools"].as_array().unwrap().iter().filter_map(|d| d["name"].as_str()).collect();
        assert_eq!(names, vec!["read"]);
        let refused: bool = eval(&h, r#"
          local result = T:execute({id="cli-tool-ceiling", name="write", arguments={path="cli-ceiling-escape", content="bad"}}, {})
          return result.refused == true
        "#).await;
        assert!(refused, "the selected main-agent type must also constrain direct tool execution");
        assert!(!env.cwd.join("cli-ceiling-escape").exists());
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn cli_agent_type_remains_active_after_reloading_the_same_session() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(json!([{ "text":"before" }, { "text":"after" }]))
            .args(json!({"agent":"explore"}))
            .build()
            .await;
        expose(&h);
        let first = h.prompt("before reload", json!({})).await.unwrap();
        assert!(first["error"].is_null(), "{first}");
        let old: String = eval(&h, "return S:info().session_id").await;
        exec(&h, "mare.reload(); C.cordis:get('kernel'):reload_barrier()").await;
        expose(&h);
        let current: String = eval(&h, "return S:info().session_id").await;
        assert_eq!(old, current, "reload must restore the original session");
        exec(
            &h,
            r#"
          C.cordis:on("request/build", function(req, next)
            RELOADED_REQUEST = mare.json.encode({system=req.system, tools=req.tools})
            return next()
          end)
        "#,
        )
        .await;
        let second = h.prompt("after reload", json!({})).await.unwrap();
        assert!(second["error"].is_null(), "{second}");
        let encoded: String = eval(&h, "return RELOADED_REQUEST").await;
        let request: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            request["system"],
            "Inspect the repository without modifying it. Return the relevant evidence and file locations."
        );
        let names: Vec<_> = request["tools"].as_array().unwrap().iter().filter_map(|d| d["name"].as_str()).collect();
        assert_eq!(names, vec!["read", "bash", "ls"]);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn unknown_cli_agent_refuses_before_consuming_first_input() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(json!([{ "text": "unexpected" }]))
            .args(json!({ "agent": "does-not-exist", "no_session": true }))
            .build()
            .await;
        expose(&h);
        let result = h.prompt("not sent", json!({})).await.unwrap();
        assert!(result["error"]["message"].as_str().unwrap_or_default().contains("unknown subagent type"), "{result}");
        assert_eq!(
            eval::<usize>(&h, "return #A:messages()").await,
            0,
            "an invalid startup type must not consume the input"
        );
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn cli_agent_requires_the_type_registry_service() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([{ "text": "unexpected" }]))
            .config(json!({"plugins":{"subagent":{"enabled":false}}}))
            .args(json!({"agent":"plan", "no_session":true}))
            .build()
            .await;
        expose(&h);
        let result = h.prompt("not sent", json!({})).await.unwrap();
        assert!(result["error"]["message"].as_str().unwrap_or_default().contains("subagent"), "{result}");
        assert_eq!(eval::<usize>(&h, "return #A:messages()").await, 0);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn cli_agent_does_not_replace_a_resumed_session_type() {
    local(async {
        let env = TestEnv::new().await;
        let first = env.host().builtins(SUBAGENT_BUILTINS).faux(json!([{ "text": "saved reply" }])).build().await;
        expose(&first);
        first.prompt("saved input", json!({})).await.unwrap();
        let id: String = eval(&first, "return S:info().session_id").await;
        first.shutdown().await;
        let resumed = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(json!([{ "text": "unexpected" }]))
            .args(json!({"agent":"plan", "resume":id}))
            .build()
            .await;
        expose(&resumed);
        let (loaded_id, before): (String, usize) = eval(&resumed, "return S:info().session_id, #A:messages()").await;
        assert_eq!(loaded_id, id);
        let result = resumed.prompt("not sent", json!({})).await.unwrap();
        assert!(result["error"]["message"].as_str().unwrap_or_default().contains("resumed session"), "{result}");
        assert_eq!(eval::<usize>(&resumed, "return #A:messages()").await, before);
        resumed.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn direct_subagent_start_inherits_cli_agent_tool_ceiling() {
    local(async {
        let env = TestEnv::new().await;
        let agents = env.config_dir().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("limited.md"), "---\ndescription: Limited\ntools: [read]\n---\nRead only").unwrap();
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .user_plugins()
            .faux(json!([{ "text": "done" }]))
            .args(json!({ "agent": "limited", "no_session": true }))
            .build()
            .await;
        expose(&h);
        let tools: String = eval(
            &h,
            r#"
          local svc = C.cordis:get("subagent")
          local original, chosen = mare.subagent.start
          mare.subagent.start = function(opts) chosen = opts.tools return original(opts) end
          local child = svc:start({prompt="child", tools={"read", "write"}})
          mare.subagent.start = original
          svc:wait(child.subagent_id, {timeout_ms=10000})
          return table.concat(chosen, ",")
        "#,
        )
        .await;
        assert_eq!(tools, "read", "direct start widened the active main agent's type ceiling");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn subagent_identity_is_the_same_in_service_and_lifecycle_events() {
    local(async {
        let env = TestEnv::new().await;
        for ephemeral in [false, true] {
            let mut builder = env.host().builtins(SUBAGENT_BUILTINS).faux(json!([{ "text": "done" }]));
            if ephemeral {
                builder = builder.no_session();
            }
            let h = builder.build().await;
            expose(&h);
            let (id, start_id, end_id, parent_ok, old_fields_absent, raw_id): (
                String,
                String,
                String,
                bool,
                bool,
                String,
            ) = eval(
                &h,
                r#"
                local svc = C.cordis:get("subagent")
                local started, completed
                local tx, rx = mare.task.channel()
                C.cordis:on("subagent/started", function(ev) started = ev end)
                C.cordis:on("subagent/completed", function(ev) completed = ev tx:send(true) end)
                local parent = S:info().session_id
                local child = svc:start({prompt = "identity"})
                local result = svc:wait(child.subagent_id, {timeout_ms = 10000})
                assert(result.status == "completed", mare.json.encode(result))
                assert(mare.task.timeout(5000, function() return rx:recv() end), "completion event missing")
                local raw = mare.subagent.get(child.subagent_id)
                return child.subagent_id, started.subagent_id or "", completed.subagent_id or "",
                  started.parent_session_id == parent and completed.parent_session_id == parent,
                  started.child_session_id == nil and started.parent_session == nil and started.id == nil
                    and completed.child_session_id == nil and completed.parent_session == nil and completed.id == nil,
                  raw.subagent_id or ""
            "#,
            )
            .await;
            assert_eq!((&start_id, &end_id, &raw_id), (&id, &id, &id), "ephemeral={ephemeral}");
            assert!(parent_ok && old_fields_absent, "ephemeral={ephemeral}");
            h.shutdown().await;
        }
    })
    .await;
}

#[tokio::test]
async fn child_type_and_explicit_tools_cannot_widen_parent_tool_constraints() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join("tool-ceiling-probe.lua"), r#"
          return { name = "tool-ceiling-probe", inject = { "bus", "tools" },
            contracts = { probe = { owner = "tool-ceiling-probe", events = {
              ["child-execute"] = { doc = "probe", mode = "emit", expose = "both", delivery = "required", ignorable = false, data = { type = "object", additionalProperties = true }, example = {} },
              ["child-tools"] = { doc = "probe", mode = "emit", expose = "both", delivery = "required", ignorable = false, data = { type = "object", additionalProperties = true }, example = {} },
            } } }, apply = function(ctx)
            if mare.mode == "child" then
              ctx.tools:register("ceiling_probe_write", {
                description = "try a forbidden write", parameters = {type="object"},
                execute = function() mare.fs.write(mare.cwd .. "/ceiling-escape", "bad") return {content="wrote"} end,
              })
              ctx:on("turn/started", function()
                local result = ctx.tools:execute({id="ceiling-probe", name="ceiling_probe_write", arguments={}})
                ctx.bus:emit("probe/child-execute", {refused=result.refused == true})
              end)
              ctx:on("request/build", function(req, next)
                local names = {} for _, tool in ipairs(req.tools) do names[#names + 1] = tool.name end
                table.sort(names)
                ctx.bus:emit("probe/child-tools", {names = names})
                return next()
              end)
            end
          end }
        "#).unwrap();
        let h = env.host().builtins(SUBAGENT_BUILTINS).user_plugins().faux(json!([{ "text": "done" }])).no_session().build().await;
        expose(&h);
        let (parent_limited, type_denied, first_refused, second_refused): (String, String, bool, bool) = eval(&h, r#"
          local svc, llm = C.cordis:get("subagent"), C.cordis:get("llm")
          local remove_type = svc:register_type({name="narrow-check", description="Tool ceiling", tools={"read", "write"}})
          local constrain = llm:constrain_tools(function() return {"read"} end)
          local spawn, handle = mare.subagent.start, nil
          mare.subagent.start = function(opts) handle = spawn(opts) return handle end
          local function run()
            local child = svc:start({type="narrow-check", prompt="test", tools={"read", "write"}})
            local names, refused
            while true do
              local event = handle:next_event()
              if not event then break end
              if event.name == "probe/child-tools" then names = event.payload.names end
              if event.name == "probe/child-execute" then refused = event.payload.refused end
              if names and refused ~= nil then break end
            end
            assert(names and refused ~= nil, "child tool declaration or execution was not observed")
            svc:wait(child.subagent_id, {timeout_ms=10000})
            return table.concat(names, ","), refused
          end
          local first, first_refused = run()
          constrain() remove_type()
          remove_type = svc:register_type({name="narrow-check", description="Tool ceiling", tools={"read", "write"}, disallowed_tools={"read"}})
          local second, second_refused = run()
          remove_type()
          return first, second, first_refused, second_refused
        "#).await;
        assert_eq!(parent_limited, "read", "a type or explicit list must not widen the parent's allowlist");
        assert_eq!(type_denied, "write", "the type denylist must override the explicit start list");
        assert!(!env.cwd.join("ceiling-escape").exists(), "a child wrote through the restricted tool");
        assert!(first_refused && second_refused, "direct tool execution must honor the ceiling");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn running_child_keeps_the_type_snapshot_after_registration_is_released() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join("type-snapshot-probe.lua"), r#"
          return {name="type-snapshot-probe", inject={"bus"},
            contracts={probe={owner="type-snapshot-probe", events={
              ["type-snapshot"]={doc="probe", mode="emit", expose="both", delivery="required", ignorable=false, data={type="object", additionalProperties=true}, example={}},
            }}}, apply=function(ctx)
            if mare.mode == "child" then
              ctx:on("request/build", function(req, next)
                ctx.bus:emit("probe/type-snapshot", {system=req.system, tools=req.tools})
                return next()
              end)
            end
          end}
        "#).unwrap();
        let h = env.host().builtins(SUBAGENT_BUILTINS).user_plugins().faux(json!([{ "text":"done" }])).build().await;
        expose(&h);
        let (missing, captured, status): (bool, String, String) = eval(&h, r#"
          local svc, spawn, handle = C.cordis:get("subagent"), mare.subagent.start
          local release = svc:register_type({name="snapshot-type", description="owned", system="IMMUTABLE-TYPE", tools={"read"}})
          mare.subagent.start = function(opts) handle=spawn(opts); return handle end
          local child = svc:start({type="snapshot-type", prompt="work"})
          mare.subagent.start = spawn
          assert(svc:get(child.subagent_id).status == "running", "child must be running before type withdrawal")
          release()
          local missing = true
          for _, def in ipairs(svc:list_types().data) do if def.name == "snapshot-type" then missing=false end end
          local captured
          while true do
            local event=handle:next_event()
            if not event then break end
            if event.name == "probe/type-snapshot" then captured=event.payload; break end
          end
          local result = svc:wait(child.subagent_id, {timeout_ms=10000})
          return missing, mare.json.encode(captured), result.status
        "#).await;
        assert!(missing, "released type must disappear from the registry");
        assert_eq!(status, "completed");
        let request: Value = serde_json::from_str(&captured).unwrap();
        assert_eq!(request["system"], "IMMUTABLE-TYPE", "{request}");
        let names: Vec<_> = request["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["read"], "{request}");
        h.shutdown().await;
    }).await;
}

#[tokio::test]
async fn fork_one_keeps_the_start_of_a_turn_that_received_steer() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(json!([
                {"tool_calls":[{"name":"hold_for_steer", "arguments":{}}]},
                {"text":"STEERED-ANSWER"}
            ]))
            .build()
            .await;
        expose(&h);
        let messages: String = eval(
            &h,
            r#"
          local entered_tx, entered_rx = mare.task.channel()
          local release_tx, release_rx = mare.task.channel()
          T:register("hold_for_steer", {description="hold for steer", side_effect="read", parameters={type="object"},
            execute=function() entered_tx:send(true); release_rx:recv(); return {content="STEER-TOOL-RESULT"} end})
          local turn = A:prompt("STEER-ORIGINAL-REQUEST")
          assert(mare.task.timeout(5000, function() return entered_rx:recv() end), "tool did not start")
          A:steer("STEER-ADDITIONAL-INSTRUCTION")
          A:prompt("QUEUED-FOLLOW-UP", {when_busy="queue"})
          release_tx:send(true)
          A:wait(turn.turn_id)
          local snapshot = S:snapshot({session_id=S:info().session_id, last_completed_turns=1})
          local context = S:snapshot_context({session_id=snapshot.session_id, allow_memory=false})
          S:delete(snapshot.session_id)
          return mare.json.encode(context.messages)
        "#,
        )
        .await;
        for value in [
            "STEER-ORIGINAL-REQUEST",
            "hold_for_steer",
            "STEER-TOOL-RESULT",
            "STEER-ADDITIONAL-INSTRUCTION",
            "STEERED-ANSWER",
            "QUEUED-FOLLOW-UP",
        ] {
            assert!(messages.contains(value), "missing {value} from selected turn: {messages}");
        }
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn fork_one_does_not_join_an_aborted_turn_to_the_next_turn() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(json!([
                {"text":"slow first", "delay_ms":60000}, {"text":"new completed answer"}
            ]))
            .build()
            .await;
        expose(&h);
        let history: String = eval(
            &h,
            r#"
          local tx, rx = mare.task.channel()
          local remove = C.cordis:on("item/started", function(ev)
            if ev.item and ev.item.type == "assistant_message" then tx:send(true) end
          end)
          local first = A:prompt("ABORTED-OLD-REQUEST")
          assert(mare.task.timeout(5000, function() return rx:recv() end), "first turn did not start")
          A:cancel()
          A:wait(first.turn_id)
          remove()
          local second = A:prompt("AFTER-ABORT-REQUEST")
          A:wait(second.turn_id)
          local snap = S:snapshot({session_id=S:info().session_id, last_completed_turns=1})
          local context = S:snapshot_context({session_id=snap.session_id, allow_memory=false})
          S:delete(snap.session_id)
          return mare.json.encode(context.messages)
        "#,
        )
        .await;
        assert!(history.contains("AFTER-ABORT-REQUEST"), "{history}");
        assert!(!history.contains("ABORTED-OLD-REQUEST"), "cancelled turn leaked: {history}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn fork_one_excludes_a_failed_provider_turn() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "error": "provider failed", "kind": "invalid_request", "retryable": false },
                { "text": "SUCCESSFUL-ANSWER" }
            ]))
            .build_exposed()
            .await;
        let history: String = eval(
            &h,
            r#"
            local first = A:prompt("FAILED-OLD-REQUEST")
            A:wait(first.turn_id)
            local second = A:prompt("SUCCESSFUL-NEW-REQUEST")
            A:wait(second.turn_id)
            local snap = S:snapshot({session_id=S:info().session_id, last_completed_turns=1})
            local context = S:snapshot_context({session_id=snap.session_id, allow_memory=false})
            S:delete(snap.session_id)
            return mare.json.encode(context.messages)
        "#,
        )
        .await;
        assert!(history.contains("SUCCESSFUL-NEW-REQUEST"), "{history}");
        assert!(history.contains("SUCCESSFUL-ANSWER"), "{history}");
        assert!(!history.contains("FAILED-OLD-REQUEST"), "failed turn leaked into fork: {history}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn fork_one_inherits_only_the_latest_completed_parent_turn() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("fork-request-probe.lua"),
            r#"return { name = "fork-request-probe", inject = { "bus" },
              contracts = { probe = { owner = "fork-request-probe", events = {
                ["fork-history"] = { doc = "probe", mode = "emit", expose = "both", delivery = "required", ignorable = false, data = { type = "object", additionalProperties = true }, example = {} },
              } } }, apply = function(ctx)
              if mare.mode == "child" then
                ctx:on("request/build", function(req, next)
                  ctx.bus:emit("probe/fork-history", {messages = req.messages})
                  return next()
                end)
              end
            end }"#,
        )
        .unwrap();
        let h = env.host().builtins(SUBAGENT_BUILTINS).user_plugins().faux(json!([{ "text": "done" }])).build().await;
        expose(&h);
        for prompt in ["FIRST-PARENT-TURN", "SECOND-PARENT-TURN"] {
            let result = h.prompt(prompt, json!({})).await.unwrap();
            assert!(result["error"].is_null(), "{result}");
        }
        let (id, messages, records, parent_id, parent_leaf): (String, String, String, String, String) = eval(
            &h,
            r#"
            local parent = S:info()
            assert(parent.path, "parent must have durable history")
            local svc = C.cordis:get("subagent")
            local spawn = mare.subagent.start
            local handle
            mare.subagent.start = function(opts) handle = spawn(opts) return handle end
            local child = svc:start({prompt = "CHILD-TASK", fork = 1})
            local messages
            while true do
              local event = handle:next_event()
              if not event then break end
              if event.name == "probe/fork-history" then messages = event.payload.messages break end
            end
            assert(messages, "child provider request was not observed")
            local result = svc:wait(child.subagent_id, {timeout_ms = 10000})
            assert(result.status == "completed", mare.json.encode(result))
            local path
            for _, entry in ipairs(S:children()) do
              if entry.name:find(child.subagent_id, 1, true) then path = entry.path end
            end
            assert(path, "the child must have its own transcript")
            local records = mare.store.read(path)
            return child.subagent_id, mare.json.encode(messages), mare.json.encode(records), parent.session_id, parent.leaf_entry_id
            "#,
        )
        .await;
        assert!(!id.is_empty());
        assert!(
            messages.contains("SECOND-PARENT-TURN") && messages.contains("CHILD-TASK"),
            "{messages}\ntranscript: {records}"
        );
        assert!(!messages.contains("FIRST-PARENT-TURN"), "fork=1 leaked an older completed turn: {messages}");
        let records: Value = serde_json::from_str(&records).unwrap();
        let records = records.as_array().unwrap();
        assert_eq!(records[0]["session_id"], id, "registry and child transcript identities differ");
        let origin_index = records.iter().position(|e| e["kind"] == "session/fork-origin").expect("fork-origin record");
        let origin = &records[origin_index]["data"];
        assert_eq!(origin["source_session_id"], parent_id);
        assert_eq!(origin["source_leaf_entry_id"], parent_leaf);
        let inherited = origin["inherited_through_entry_id"].as_str().expect("inherited boundary");
        let inherited_index = records.iter().position(|e| e["entry_id"] == inherited).expect("boundary message");
        assert!(origin_index < inherited_index, "origin must precede the inherited seed");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn nested_fork_from_a_recorded_child_uses_its_own_completed_history() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("nested-fork-probe.lua"),
            r#"
          return { name="nested-fork-probe", inject={"bus", "subagent"},
            contracts={probe={owner="nested-fork-probe", events={
              ["nested-grandchild"]={doc="probe", mode="emit", expose="both", delivery="required", ignorable=false, data={type="object", additionalProperties=true}, example={}},
              ["nested-result"]={doc="probe", mode="emit", expose="both", delivery="required", ignorable=false, data={type="object", additionalProperties=true}, example={}},
            }}}, apply=function(ctx)
            if mare.mode ~= "child" then return end
            local depth = mare.config.nested_probe_depth or 1
            ctx:on("request/build", function(req, next)
              if depth == 2 then
                ctx.bus:emit("probe/nested-grandchild", {messages=req.messages})
              elseif depth == 1 then
                local ok, result, grandchild_id, history_model, history_effort = pcall(function()
                  local spawn, handle = mare.subagent.start
                  local model, effort
                  mare.subagent.start = function(opts)
                    model, effort = opts._history_model, opts._history_effort
                    handle = spawn(opts); return handle
                  end
                  local child = ctx.subagent:start({prompt="NESTED-GRANDCHILD", fork=1, config={nested_probe_depth=2}})
                  mare.subagent.start = spawn
                  local messages
                  while true do
                    local event = handle:next_event()
                    if not event then break end
                    if event.name == "probe/nested-grandchild" then messages = event.payload.messages; break end
                  end
                  local done = ctx.subagent:wait(child.subagent_id, {timeout_ms=10000})
                  assert(done.status == "completed" and messages, mare.json.encode(done))
                  return mare.json.encode(messages), child.subagent_id, model, effort
                end)
                ctx.bus:emit("probe/nested-result", {ok=ok, result=tostring(result), grandchild_id=grandchild_id,
                  history_model=history_model, history_effort=history_effort})
              end
              return next()
            end)
          end }
        "#,
        )
        .unwrap();
        let h = env.host().builtins(SUBAGENT_BUILTINS).user_plugins().faux(json!([{ "text":"done" }])).build().await;
        expose(&h);
        exec(&h, "C.cordis:get('llm'):set_effort('high', {target='session', save=false})").await;
        h.prompt("NESTED-OLDER-PARENT", json!({})).await.unwrap();
        h.prompt("NESTED-RECENT-PARENT", json!({})).await.unwrap();
        let (ok, result, child_id, grandchild_id, model, effort): (bool, String, String, String, String, String) =
            eval(
                &h,
                r#"
          local svc, spawn, handle = C.cordis:get("subagent"), mare.subagent.start
          mare.subagent.start = function(opts) handle=spawn(opts); return handle end
          local child = svc:start({prompt="NESTED-CHILD", fork="all"})
          mare.subagent.start = spawn
          local observed
          while true do
            local event=handle:next_event()
            if not event then break end
            if event.name == "probe/nested-result" then observed=event.payload; break end
          end
          svc:wait(child.subagent_id, {timeout_ms=10000})
          assert(observed, "nested request was not observed")
          return observed.ok, observed.result, child.subagent_id, observed.grandchild_id or "",
            observed.history_model or "", observed.history_effort or ""
        "#,
            )
            .await;
        assert!(ok, "nested fork failed: {result}");
        assert_eq!(
            (model.as_str(), effort.as_str()),
            ("faux/echo", "high"),
            "nested fork must use the source snapshot settings"
        );
        assert!(result.contains("NESTED-RECENT-PARENT") && result.contains("NESTED-GRANDCHILD"), "{result}");
        assert!(!result.contains("NESTED-OLDER-PARENT") && !result.contains("NESTED-CHILD"), "{result}");
        let files = mare_store::index::session_files(&env.home.path().join("state/sessions"));
        let child_path = files.iter().find(|p| p.to_string_lossy().contains(&child_id)).expect("indexed child path");
        let grandchild_path = files
            .iter()
            .find(|p| p.to_string_lossy().contains(&grandchild_id))
            .expect("nested child transcript must be discoverable by the normal session index");
        let nested = mare_store::read_fork_snapshot(grandchild_path, Some(&grandchild_id)).unwrap();
        assert_eq!(nested[0]["parent_session_id"], child_id, "{nested:?}");
        let origin = nested.iter().find(|e| e["kind"] == "session/fork-origin").unwrap();
        assert_eq!(origin["data"]["source_session_id"], child_id, "{nested:?}");
        assert_ne!(grandchild_path, child_path);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn concurrent_parent_forks_keep_their_own_requested_turns() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("fork-isolation-probe.lua"),
            r#"
          return { name="fork-isolation-probe", inject={"bus"},
            contracts={probe={owner="fork-isolation-probe", events={
              ["fork-isolation"]={doc="probe", mode="emit", expose="both", delivery="required", ignorable=false, data={type="object", additionalProperties=true}, example={}},
            }}}, apply=function(ctx)
            if mare.mode == "child" then
              ctx:on("request/build", function(req, next)
                ctx.bus:emit("probe/fork-isolation", {messages=req.messages})
                return next()
              end)
            end
          end }
        "#,
        )
        .unwrap();
        let h1 = env.host().builtins(SUBAGENT_BUILTINS).user_plugins().faux(json!([{ "text": "done" }])).build().await;
        let h2 = env.host().builtins(SUBAGENT_BUILTINS).user_plugins().faux(json!([{ "text": "done" }])).build().await;
        expose(&h1);
        expose(&h2);
        for text in ["PARENT-ONE-FIRST", "PARENT-ONE-SECOND", "PARENT-ONE-THIRD"] {
            let result = h1.prompt(text, json!({})).await.unwrap();
            assert!(result["error"].is_null(), "{result}");
        }
        for text in ["PARENT-TWO-FIRST", "PARENT-TWO-SECOND"] {
            let result = h2.prompt(text, json!({})).await.unwrap();
            assert!(result["error"].is_null(), "{result}");
        }
        let install = r#"
          function CAPTURE_FORK(fork, prompt)
            local svc, original, handle = C.cordis:get("subagent"), mare.subagent.start
            mare.subagent.start = function(opts) handle = original(opts) return handle end
            local child = svc:start({prompt=prompt, fork=fork})
            mare.subagent.start = original
            local messages
            while true do
              local event = handle:next_event()
              if not event then break end
              if event.name == "probe/fork-isolation" then messages = event.payload.messages break end
            end
            assert(messages, "child request missing")
            local result = svc:wait(child.subagent_id, {timeout_ms=10000})
            assert(result.status == "completed", mare.json.encode(result))
            return mare.json.encode(messages)
          end
        "#;
        exec(&h1, install).await;
        exec(&h2, install).await;
        let (last_two, other_parent) = tokio::join!(
            eval::<String>(&h1, "return CAPTURE_FORK(2, 'CHILD-ONE')"),
            eval::<String>(&h2, "return CAPTURE_FORK('all', 'CHILD-TWO')"),
        );
        assert!(last_two.contains("PARENT-ONE-SECOND") && last_two.contains("PARENT-ONE-THIRD"), "{last_two}");
        assert!(!last_two.contains("PARENT-ONE-FIRST") && !last_two.contains("PARENT-TWO"), "{last_two}");
        assert!(
            other_parent.contains("PARENT-TWO-FIRST") && other_parent.contains("PARENT-TWO-SECOND"),
            "{other_parent}"
        );
        assert!(!other_parent.contains("PARENT-ONE"), "{other_parent}");
        let all_available: String = eval(&h1, "return CAPTURE_FORK(99, 'CHILD-ALL')").await;
        let none: String = eval(&h1, "return CAPTURE_FORK('none', 'CHILD-NONE')").await;
        assert!(
            all_available.contains("PARENT-ONE-FIRST") && all_available.contains("PARENT-ONE-THIRD"),
            "{all_available}"
        );
        assert!(!none.contains("PARENT-ONE"), "{none}");
        h1.shutdown().await;
        h2.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn fork_uses_snapshot_model_and_effort_when_parent_changes_after_snapshot() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("history-model-probe.lua"),
            r#"
          return {name="history-model-probe", inject={"bus"},
            contracts={probe={owner="history-model-probe", events={
              ["history-model"]={doc="probe", mode="emit", expose="both", delivery="required", ignorable=false, data={type="object", additionalProperties=true}, example={}},
            }}}, apply=function(ctx)
            if mare.mode == "child" then
              ctx:on("llm/model-changed", function(event)
                ctx.bus:emit("probe/history-model", {model=event.model.ref, effort=event.effort})
              end)
            end
          end }
        "#,
        )
        .unwrap();
        let h = env.host().builtins(SUBAGENT_BUILTINS).user_plugins().faux(json!([{ "text": "done" }])).build().await;
        expose(&h);
        exec(&h, "C.cordis:get('llm'):set_effort('high', {target='session', save=false})").await;
        let result = h.prompt("source history", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let (snapshot_model, snapshot_effort, current_model, passed_model, passed_effort, events): (
            String,
            String,
            String,
            String,
            String,
            String,
        ) = eval(
            &h,
            r#"
          local llm, svc = C.cordis:get("llm"), C.cordis:get("subagent")
          local old_context, old_spawn = S.snapshot_context, mare.subagent.start
          local snapshot_model, snapshot_effort, passed_model, passed_effort, handle
          S.snapshot_context = function(self, opts)
            local result = old_context(self, opts)
            snapshot_model, snapshot_effort = result.model, result.effort
            llm:set_model("faux/echo2", {target="session", save=false})
            return result
          end
          mare.subagent.start = function(opts)
            passed_model, passed_effort = opts._history_model, opts._history_effort
            handle = old_spawn(opts)
            return handle
          end
          local child = svc:start({prompt="child", fork="all", model="faux/echo2"})
          S.snapshot_context, mare.subagent.start = old_context, old_spawn
          local seen = mare.json.array()
          while true do
            local event = handle:next_event()
            if not event then break end
            if event.name == "probe/history-model" then seen[#seen + 1] = event.payload end
          end
          local done = svc:wait(child.subagent_id, {timeout_ms=10000})
          assert(done.status == "completed", mare.json.encode(done))
          return snapshot_model or "", snapshot_effort or "", llm:current().ref,
            passed_model or "", passed_effort or "", mare.json.encode(seen)
        "#,
        )
        .await;
        assert_eq!((snapshot_model.as_str(), snapshot_effort.as_str()), ("faux/echo", "high"));
        assert_eq!(current_model, "faux/echo2");
        assert_eq!((passed_model.as_str(), passed_effort.as_str()), ("faux/echo", "high"));
        let events: Value = serde_json::from_str(&events).unwrap();
        assert!(
            events.as_array().unwrap().iter().any(|e| e["model"] == "faux/echo" && e["effort"] == "high"),
            "{events}"
        );
        assert!(events.as_array().unwrap().iter().any(|e| e["model"] == "faux/echo2"), "{events}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn fork_rejects_ephemeral_history_and_explicit_messages() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(SUBAGENT_BUILTINS).faux(json!([])).no_session().build().await;
        expose(&h);
        let (code, message, conflict, count): (String, String, String, usize) = eval(
            &h,
            r#"
          local svc = C.cordis:get("subagent")
          local ok, err = pcall(svc.start, svc, {prompt="x", fork="all"})
          assert(not ok)
          local conflict_ok, conflict_err = pcall(svc.start, svc, {prompt="x", fork=1, messages=mare.json.array()})
          assert(not conflict_ok)
          return err.code, err.message, conflict_err.code, #svc:list().data
        "#,
        )
        .await;
        assert_eq!(code, "unavailable");
        assert!(message.contains("history"), "{message}");
        assert_eq!(conflict, "invalid_arguments");
        assert_eq!(count, 0, "failed preparation must not create a child");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn snapshot_preparation_failure_reports_undeleted_temporary_id() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(json!([{ "text": "done" }]))
            .config(json!({"agents":{"max_active":1}}))
            .build()
            .await;
        expose(&h);
        h.prompt("parent", json!({})).await.unwrap();
        let (code, message, snapshot, child_count, retry): (String, String, String, usize, String) = eval(
            &h,
            r#"
          local svc = C.cordis:get("subagent")
          local old_snapshot, old_context, old_delete = S.snapshot, S.snapshot_context, S.delete
          local snapshot_id
          S.snapshot = function(self, opts)
            local result = old_snapshot(self, opts)
            snapshot_id = result.session_id
            return result
          end
          S.snapshot_context = function() error("injected context failure") end
          S.delete = function(self, id)
            if id == snapshot_id then error("injected deletion failure") end
            return old_delete(self, id)
          end
          local ok, err = pcall(svc.start, svc, {prompt="child", fork="all", name="reusable-name"})
          assert(not ok)
          local count = #svc:list().data
          S.snapshot, S.snapshot_context, S.delete = old_snapshot, old_context, old_delete
          S:delete(snapshot_id)
          local retried = svc:start({prompt="retry child", fork="all", name="reusable-name"})
          local done = svc:wait(retried.subagent_id, {timeout_ms=10000})
          return err.code, err.message, snapshot_id, count, done.status
        "#,
        )
        .await;
        assert_eq!(code, "internal");
        assert!(message.contains(&snapshot), "{message}");
        assert!(message.contains("injected context failure"), "{message}");
        assert_eq!(child_count, 0);
        assert_eq!(retry, "completed", "failure must release both the name and the only child slot");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn temporary_snapshot_deletion_failure_keeps_the_started_child_and_warns() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(SUBAGENT_BUILTINS).faux(json!([{ "text": "done" }])).build().await;
        expose(&h);
        h.prompt("parent", json!({})).await.unwrap();
        let (child, snapshot, visible, warned, status): (String, String, bool, bool, String) = eval(
            &h,
            r#"
          local svc = C.cordis:get("subagent")
          local old_snapshot, old_delete, old_log = S.snapshot, S.delete, mare.log
          local snapshot_id, visible, warned
          S.snapshot = function(self, opts)
            local result = old_snapshot(self, opts)
            snapshot_id = result.session_id
            visible = S:get(snapshot_id).session_id == snapshot_id
            return result
          end
          S.delete = function(self, id)
            if id == snapshot_id then error("injected deletion failure") end
            return old_delete(self, id)
          end
          mare.log = function(level, text)
            if level == "warn" and snapshot_id and text:find(snapshot_id, 1, true) then warned = true end
            return old_log(level, text)
          end
          local child = svc:start({prompt="child", fork="all"})
          local result = svc:wait(child.subagent_id, {timeout_ms=10000})
          local remains = S:get(snapshot_id).session_id == snapshot_id
          S.snapshot, S.delete, mare.log = old_snapshot, old_delete, old_log
          S:delete(snapshot_id)
          return child.subagent_id, snapshot_id, visible and remains, warned == true, result.status
        "#,
        )
        .await;
        assert_ne!(child, snapshot, "temporary and child identities must differ");
        assert!(visible && warned, "temporary snapshot must remain visible with a warning on cleanup failure");
        assert_eq!(status, "completed");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn snapshot_of_inflight_tool_adds_only_the_initiator_pending_result() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([
          {"text":"old reply"}, {"text":"recent reply"},
          {"tool_calls":[{"name":"hold_parent", "arguments":{}}]},
          {"text":"parent finished"}
        ])).build().await;
        expose(&h);
        h.prompt("OLDEST-PARENT", json!({})).await.unwrap();
        h.prompt("RECENT-PARENT", json!({})).await.unwrap();
        let (with_call, without_call, rejected, parent_result): (String, String, String, String) = eval(&h, r#"
          local entered_tx, entered_rx = mare.task.channel()
          local release_tx, release_rx = mare.task.channel()
          T:register("hold_parent", {description="hold", side_effect="read", parameters={type="object"},
            execute=function(_, tool_ctx) entered_tx:send(tool_ctx.tool_call_id); release_rx:recv(); return {content="parent actual result"} end})
          local task = mare.task.spawn(function() return A:wait(A:prompt("CURRENT-PARENT").turn_id) end)
          local entered, call_id = mare.task.timeout(5000, function() return entered_rx:recv() end)
          assert(entered and type(call_id) == "string", "parent tool did not start")
          local id = S:info().session_id
          local snap = S:snapshot({session_id=id, last_completed_turns=1, initiating_tool_call_id=call_id, request_id="with-call"})
          local repeated = S:snapshot({session_id=id, last_completed_turns=1, initiating_tool_call_id=call_id, request_id="with-call"})
          assert(repeated.session_id == snap.session_id, "identical snapshot receipt must be reused")
          for _, changed in ipairs({
            {session_id=id, last_completed_turns=2, initiating_tool_call_id=call_id, request_id="with-call"},
            {session_id=id, last_completed_turns=1, request_id="with-call"},
          }) do
            local ok, err = pcall(S.snapshot, S, changed)
            assert(not ok and err.code == "request_id_conflict", "new snapshot fields must be part of the receipt fingerprint")
          end
          local context = S:snapshot_context({session_id=snap.session_id, allow_memory=false})
          local direct = S:snapshot({session_id=id, last_completed_turns=1})
          local direct_context = S:snapshot_context({session_id=direct.session_id, allow_memory=false})
          local ok, err = pcall(S.snapshot, S, {session_id=id, last_completed_turns=1, initiating_tool_call_id="not-pending"})
          assert(not ok)
          local error_code = err.code
          assert(A:get().busy, "snapshot must leave the parent running")
          release_tx:send(true)
          local parent = task:wait()
          S:delete(snap.session_id) S:delete(direct.session_id)
          return mare.json.encode(context.messages), mare.json.encode(direct_context.messages), error_code, parent.text
        "#).await;
        assert!(with_call.contains("RECENT-PARENT") && with_call.contains("CURRENT-PARENT"), "{with_call}");
        assert!(!with_call.contains("OLDEST-PARENT") && !with_call.contains("parent actual result"), "{with_call}");
        assert!(with_call.contains("The parent is still executing this tool call; its result is pending in the parent session."), "{with_call}");
        assert!(without_call.contains("RECENT-PARENT") && !without_call.contains("CURRENT-PARENT"), "{without_call}");
        assert_eq!(rejected, "invalid_arguments");
        assert_eq!(parent_result, "parent finished");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn snapshot_preserves_completed_and_classifies_parallel_pending_tool_results() {
    local(async {
        let env = TestEnv::new().await;
        let call = |id: &str, name: &str| json!({"type":"tool_call", "id":id, "name":name, "arguments":{}});
        let source = env.write_session("2026-01-01T00-00-00_parallel.jsonl", &[
            msg("m1", None, "user", "parallel work"),
            json!({"type":"message", "entry_id":"m2", "parent_entry_id":"m1", "message":{"role":"assistant", "content":[
                call("done", "read"), call("prepared", "write"), call("started", "bash"),
                call("reading", "read"), call("initiator", "subagent")
            ]}}),
            json!({"type":"tool_state", "tool_call_id":"prepared", "name":"write", "state":"prepared", "side_effect":"write"}),
            json!({"type":"tool_state", "tool_call_id":"started", "name":"bash", "state":"started", "side_effect":"external"}),
            json!({"type":"tool_state", "tool_call_id":"reading", "name":"read", "state":"started", "side_effect":"read"}),
            json!({"type":"tool_state", "tool_call_id":"initiator", "name":"subagent", "state":"started", "side_effect":"external"}),
            json!({"type":"message", "entry_id":"m3", "parent_entry_id":"m2", "message":{"role":"tool_result", "tool_call_id":"done", "tool_name":"read", "content":[{"type":"text", "text":"already completed"}]}}),
        ]);
        let before = std::fs::read(&source).unwrap();
        let h = env.host().build().await;
        expose(&h);
        let encoded: String = eval(&h, r#"
          S:list({}) -- make the inactive source discoverable by id
          local snap = S:snapshot({session_id="s1", initiating_tool_call_id="initiator"})
          local context = S:snapshot_context({session_id=snap.session_id, allow_memory=false})
          S:delete(snap.session_id)
          return mare.json.encode(context.messages)
        "#).await;
        let messages: Value = serde_json::from_str(&encoded).unwrap();
        let results: Vec<_> = messages.as_array().unwrap().iter().filter(|m| m["role"] == "tool_result").collect();
        assert_eq!(results.iter().map(|m| m["tool_call_id"].as_str().unwrap()).collect::<Vec<_>>(),
            ["done", "prepared", "started", "reading", "initiator"], "{messages}");
        let text = |i: usize| results[i]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text(0), "already completed");
        assert!(text(1).starts_with("interrupted: this call never started"), "{messages}");
        assert!(text(2).starts_with("outcome_unknown:"), "{messages}");
        assert!(text(3).starts_with("interrupted:"), "{messages}");
        assert_eq!(text(4), "The parent is still executing this tool call; its result is pending in the parent session.");
        assert_eq!(std::fs::read(&source).unwrap(), before, "snapshot must not write synthetic results into its source");
        h.shutdown().await;
    }).await;
}

#[tokio::test]
async fn model_subagent_tool_fork_closes_only_its_own_pending_call() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join("model-fork-probe.lua"), r#"
          return {name="model-fork-probe", inject={"bus"},
            contracts={probe={owner="model-fork-probe", events={
              ["model-fork"]={doc="probe", mode="emit", expose="both", delivery="required", ignorable=false, data={type="object", additionalProperties=true}, example={}},
            }}}, apply=function(ctx)
            if mare.mode == "child" then
              ctx:on("request/build", function(req, next)
                ctx.bus:emit("probe/model-fork", {messages=req.messages})
                return next()
              end)
            end
          end}
        "#).unwrap();
        let h = env.host().builtins(SUBAGENT_BUILTINS).user_plugins().faux(json!([
            {"text":"old reply"}, {"text":"recent reply"},
            {"tool_calls":[{"name":"subagent", "arguments":{"prompt":"child task", "type":"explore", "fork":1, "wait":true}}]},
            {"text":"parent finished"}
        ])).build().await;
        expose(&h);
        h.prompt("OLD-PARENT", json!({})).await.unwrap();
        h.prompt("RECENT-PARENT", json!({})).await.unwrap();
        exec(&h, r#"
          local original = mare.subagent.start
          mare.subagent.start = function(opts) MODEL_FORK_HANDLE = original(opts) return MODEL_FORK_HANDLE end
        "#).await;
        let result = h.prompt("CURRENT-PARENT", json!({})).await.unwrap();
        assert_eq!(result["text"], "parent finished", "{result}");
        assert!(!result.to_string().contains("failed to start subagent"), "{result}");
        let (child_history, parent_history): (String, String) = eval(&h, r#"
          local history
          while true do
            local event = MODEL_FORK_HANDLE:next_event()
            if not event then break end
            if event.name == "probe/model-fork" then history = event.payload.messages break end
          end
          assert(history, "child request missing")
          return mare.json.encode(history), mare.json.encode(A:messages())
        "#).await;
        assert!(child_history.contains("RECENT-PARENT") && child_history.contains("CURRENT-PARENT"), "{child_history}");
        assert!(!child_history.contains("OLD-PARENT"), "{child_history}");
        let pending = "The parent is still executing this tool call; its result is pending in the parent session.";
        assert_eq!(child_history.matches(pending).count(), 1, "{child_history}");
        assert!(!child_history.contains("The user interrupted"), "a live parent was not interrupted: {child_history}");
        assert!(!parent_history.contains(pending), "synthetic result leaked into parent: {parent_history}");
        h.shutdown().await;
    }).await;
}

#[tokio::test]
async fn forked_child_keeps_its_image_after_source_file_is_removed() {
    local(async {
        let env = TestEnv::new().await;
        let image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";
        let h = env.host().builtins(SUBAGENT_BUILTINS).faux(json!([{ "text": "done" }])).build().await;
        expose(&h);
        let result = h
            .prompt("IMAGE-SOURCE", json!({"images":[{"type":"image", "mime":"image/png", "data":image}]}))
            .await
            .unwrap();
        assert!(result["error"].is_null(), "{result}");
        let (parent, child): (String, String) = eval(
            &h,
            r#"
          local parent = S:info().path
          local svc = C.cordis:get("subagent")
          local child = svc:start({prompt="inspect", fork="all"})
          assert(svc:wait(child.subagent_id, {timeout_ms=10000}).status == "completed")
          for _, entry in ipairs(S:children()) do
            if entry.name:find(child.subagent_id, 1, true) then return parent, entry.path end
          end
          error("child transcript missing")
        "#,
        )
        .await;
        h.shutdown().await;
        let parent_body = std::fs::read_to_string(&parent).unwrap();
        assert!(parent_body.contains("attachment"), "the source must exercise persisted attachments");
        std::fs::remove_file(&parent).unwrap();
        let attachments = std::path::PathBuf::from(parent.trim_end_matches(".jsonl").to_owned() + ".attachments");
        std::fs::remove_dir_all(attachments).unwrap();
        let records = mare_store::read_entries(std::path::Path::new(&child)).unwrap();
        let inherited_image = records
            .iter()
            .filter_map(|entry| entry["message"]["content"].as_array())
            .flatten()
            .find(|block| block["type"] == "image")
            .expect("inherited image");
        assert_eq!(inherited_image["data"], image, "the child must own independent image data");
    })
    .await;
}

#[tokio::test]
async fn cancelling_model_fork_during_snapshot_cleans_the_temporary_session() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(json!([
                {"tool_calls":[{"name":"subagent", "arguments":{"prompt":"child", "name":"cancel-retry", "fork":1, "wait":true}}]},
                {"text":"should not be needed"}
            ]))
            .config(json!({"agents":{"max_active":1,"max_depth":1}}))
            .build()
            .await;
        expose(&h);
        let (cleaned, child_count, retry): (bool, usize, String) = eval(
            &h,
            r#"
          local entered_tx, entered_rx = mare.task.channel()
          local release_tx, release_rx = mare.task.channel()
          local original = S.snapshot_context
          S.snapshot_context = function(self, opts)
            entered_tx:send(opts.session_id)
            release_rx:recv()
            return original(self, opts)
          end
          local turn = A:prompt("fork while running")
          local entered, snapshot_id = mare.task.timeout(5000, function() return entered_rx:recv() end)
          assert(entered and type(snapshot_id) == "string", "snapshot did not reach the held read")
          A:cancel()
          release_tx:send(true)
          A:wait(turn.turn_id)
          local gone = mare.task.timeout(5000, function()
            while pcall(S.get, S, snapshot_id) do mare.task.yield() end
          end)
          S.snapshot_context = original
          local svc = C.cordis:get("subagent")
          local count = #svc:list().data
          local child = svc:start({prompt="retry", name="cancel-retry", fork="all"})
          local result = svc:wait(child.subagent_id, {timeout_ms=10000})
          return gone, count, result.status
        "#,
        )
        .await;
        assert!(cleaned, "the temporary snapshot must be removed after cancellation");
        assert_eq!(child_count, 0, "cancellation during preparation must not start a child");
        assert_eq!(retry, "completed", "cancelled preparation must release its name and capacity");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn snapshot_of_no_completed_turns_has_empty_persistent_context() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
              {"tool_calls":[{"name":"hold_first", "arguments":{}}]}, {"text":"finished"}
            ]))
            .build()
            .await;
        expose(&h);
        let (count, persisted, same_leaf): (usize, bool, bool) = eval(
            &h,
            r#"
          local entered_tx, entered_rx = mare.task.channel()
          local release_tx, release_rx = mare.task.channel()
          T:register("hold_first", {description="hold", side_effect="read", parameters={type="object"},
            execute=function() entered_tx:send(true); release_rx:recv(); return {content="done"} end})
          local task = mare.task.spawn(function() return A:wait(A:prompt("first turn").turn_id) end)
          assert(mare.task.timeout(5000, function() return entered_rx:recv() end), "parent tool did not start")
          local snap = S:snapshot({session_id=S:info().session_id, last_completed_turns=1})
          local context = S:snapshot_context({session_id=snap.session_id, allow_memory=false})
          release_tx:send(true)
          task:wait()
          S:delete(snap.session_id)
          return #context.messages, context.persisted, context.leaf_entry_id == snap.leaf_entry_id
        "#,
        )
        .await;
        assert_eq!(count, 0);
        assert!(persisted && same_leaf);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn snapshot_preserves_120001_entry_history_in_order_without_recursive_stack_growth() {
    local(async {
        let env = TestEnv::new().await;
        let count = 120_001usize;
        let mut entries = Vec::with_capacity(count + 1);
        for i in 0..count {
            entries.push(json!({
                "type": "message", "entry_id": format!("m{i}"),
                "parent_entry_id": (i > 0).then(|| format!("m{}", i - 1)),
                "created_at_ms": i,
                "message": {"role": if i == 0 { "user" } else { "assistant" },
                    "content": [{"type":"text", "text":format!("item-{i}")}]}
            }));
        }
        entries.push(json!({"type":"custom", "kind":"agent/run-end",
            "data":{"session_id":"s1", "run_id":"long", "leaf_entry_id":format!("m{}", count - 1)}}));
        let path = env.write_durable_session("long-history.jsonl", &entries);
        drop(entries);
        let h = env.host().builtins(SUBAGENT_BUILTINS).faux(json!([])).build().await;
        expose(&h);
        global(&h, "LONG_HISTORY", path.to_string_lossy().to_string());
        let (length, ordered): (usize, bool) = eval(
            &h,
            r#"
          S:load({path=LONG_HISTORY})
          local snapshot = S:snapshot({session_id=S:info().session_id, last_completed_turns=1})
          local context = S:snapshot_context({session_id=snapshot.session_id, allow_memory=false})
          local ordered = true
          mare.task.yield()
          for i, message in ipairs(context.messages) do
            if message.content[1].text ~= "item-" .. tostring(i - 1) then ordered = false break end
            if i % 1000 == 0 then mare.task.yield() end
          end
          S:delete(snapshot.session_id)
          return #context.messages, ordered
        "#,
        )
        .await;
        assert_eq!(length, count);
        assert!(ordered, "snapshot reordered the source history");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn fork_delivers_history_larger_than_the_rpc_budget_to_child() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("large-fork-probe.lua"),
            r#"
          return {name="large-fork-probe", inject={"bus"},
            contracts={probe={owner="large-fork-probe", events={
              ["large-fork"]={doc="probe", mode="emit", expose="both", delivery="required", ignorable=false, data={type="object", additionalProperties=true}, example={}},
            }}}, apply=function(ctx)
            if mare.mode == "child" then
              ctx:on("request/build", function(req, next)
                ctx.bus:emit("probe/large-fork", {count=#req.messages, first=req.messages[1].content[1].text,
                  last_source=req.messages[2001].content[1].text})
                return next()
              end)
            end
          end}
        "#,
        )
        .unwrap();
        let count = 2_001usize;
        let mut entries = vec![json!({"type":"model", "model":"faux/echo", "at_entry_id":false})];
        for i in 0..count {
            entries.push(json!({"type":"message", "entry_id":format!("m{i}"),
                "parent_entry_id":(i > 0).then(|| format!("m{}", i - 1)), "created_at_ms":i,
                "message":{"role":if i == 0 {"user"} else {"assistant"},
                    "content":[{"type":"text", "text":format!("source-{i}")}]} }));
        }
        entries.push(json!({"type":"custom", "kind":"agent/run-end",
            "data":{"session_id":"s1", "run_id":"large", "leaf_entry_id":format!("m{}",count - 1)}}));
        let path = env.write_durable_session("large-child-history.jsonl", &entries);
        let h = env.host().builtins(SUBAGENT_BUILTINS).user_plugins().faux(json!([{ "text": "done" }])).build().await;
        expose(&h);
        global(&h, "LARGE_HISTORY", path.to_string_lossy().to_string());
        let (status, error, observed, first, last): (String, String, usize, String, String) = eval(
            &h,
            r#"
          S:load({path=LARGE_HISTORY})
          local svc, original, handle = C.cordis:get("subagent"), mare.subagent.start
          mare.subagent.start = function(opts) handle = original(opts) return handle end
          local child = svc:start({prompt="child", fork="all"})
          mare.subagent.start = original
          local seen
          while true do
            local event = handle:next_event()
            if not event then break end
            if event.name == "probe/large-fork" then seen = event.payload break end
          end
          local result = svc:wait(child.subagent_id, {timeout_ms=10000})
          return result.status, result.error or "", seen and seen.count or 0, seen and seen.first or "",
            seen and seen.last_source or ""
        "#,
        )
        .await;
        assert_eq!(status, "completed", "{error}");
        assert!(observed > count);
        assert_eq!(first, "source-0");
        assert_eq!(last, "source-2000");
        h.shutdown().await;
    })
    .await;
}

#[test]
fn subagent_contract_is_full_and_lists_every_provider_method() {
    let lua = mlua::Lua::new();
    let value: mlua::Value = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
    let contracts = mare_kernel::contracts::parse_contracts(&lua, value).unwrap();
    let contract = contracts.get("subagent").expect("subagent contract exists");
    assert!(contract.owner.is_none(), "subagent contract must be a service contract");

    let expected = ["start", "register_type", "list_types", "drain", "list", "get", "wait", "steer", "cancel"];
    assert_eq!(contract.methods.len(), expected.len(), "unexpected method count: {:?}", contract.methods.keys());
    for name in expected {
        let method = contract.methods.get(name).unwrap_or_else(|| panic!("missing method `{name}`"));
        assert_eq!(method.remote(), name != "drain", "subagent.{name} exposure differs");
        assert!(!method.doc.is_empty(), "subagent.{name} must be documented");
        if name != "drain" {
            let access = if name == "register_type" { "primary" } else { "peer" };
            assert_eq!(method.access_name(), access, "subagent.{name} has incorrect access");
        }
        assert_eq!(method.scope_name(), "session", "subagent.{name} must be session-scoped");
    }

    let provider: mlua::Table = lua
        .load(
            r#"
            local captured
            mare = mare or {}
            mare.task = { channel = function() return {}, {} end }
            package.loaded['subagent.types'] = function()
                return { get = function() end, list = function() end, register = function() end }
            end
            package.loaded['subagent.discovery'] = function() end
            package.loaded['mare_lib'] = {
                followers = function() return {} end,
                fail = function(code, message) error({ code = code, message = message }, 0) end,
                service = function(_, name, value)
                    assert(name == 'subagent')
                    captured = value
                    return value
                end,
            }
            local ctx = {
                tools = { register = function() end },
                provide = function() end,
                spawn = function() end,
                inject = function() end,
                on = function() end,
            }
            local plugin = assert(load([[PLUGIN_SRC]], '=subagent'))()
            plugin.apply(ctx, { notify_inline_bytes = 4096 })
            assert(captured, 'subagent provider was not published')
            return captured
            "#
            .replace("PLUGIN_SRC", include_str!("../../../../mare-plugin/preset/plugins/subagent.lua")),
        )
        .eval()
        .unwrap();

    let mut undeclared = Vec::new();
    for pair in provider.pairs::<mlua::Value, mlua::Value>() {
        let (key, value) = pair.unwrap();
        if let (mlua::Value::String(key), mlua::Value::Function(_)) = (key, value) {
            let name = key.to_string_lossy();
            if !name.starts_with('_') && !contract.methods.contains_key(&name) {
                undeclared.push(name);
            }
        }
    }
    undeclared.sort();
    assert!(undeclared.is_empty(), "subagent provider methods missing from contract: {undeclared:?}");
}

#[test]
fn larger_history_result_budget_is_confined_to_typed_lua_methods() {
    let lua = mlua::Lua::new();
    let contract = |method: &str| {
        let source = format!("return {{history_test={{methods={{read={method}}}}}}}");
        let value = lua.load(&source).eval::<mlua::Value>().unwrap();
        mare_kernel::contracts::parse_contracts(&lua, value)
    };
    let valid = contract(r#"{doc="read", expose="lua", history_result=true, params={type="object", additionalProperties=false}, result={type="array", items={type="object"}}}"#).unwrap();
    assert!(valid["history_test"].methods["read"].history_result);
    for invalid in [
        r#"{doc="read", expose="both", access="peer", history_result=true, params={type="object", additionalProperties=false}, result={type="object"}}"#,
        r#"{doc="read", expose="lua", history_result=true, registration=true, params={type="object", additionalProperties=false}}"#,
    ] {
        let error = contract(invalid).unwrap_err().to_string();
        assert!(error.contains("history_result"), "must refuse {invalid}: {error}");
    }
}

#[tokio::test]
async fn lua_kind_errors_contract_flag_controls_raw_kind_error_passthrough() {
    local(async {
        let lua = mlua::Lua::new();
        let contract = |method: &str| {
            let source = format!("return {{custom_stream={{methods={{run={method}}}}}}}");
            let value = lua.load(&source).eval::<mlua::Value>().unwrap();
            mare_kernel::contracts::parse_contracts(&lua, value)
        };
        let valid = contract(
            r#"{doc="run", expose="lua", lua_unchecked=true, lua_kind_errors=true, params={type="object", additionalProperties=false}, result={type="object"}}"#,
        )
        .unwrap();
        assert!(valid["custom_stream"].methods["run"].lua_kind_errors);
        for invalid in [
            r#"{doc="run", expose="both", access="peer", lua_kind_errors=true, params={type="object", additionalProperties=false}, result={type="object"}}"#,
            r#"{doc="run", expose="lua", lua_kind_errors=true, registration=true, params={type="object", additionalProperties=false}}"#,
        ] {
            let error = contract(invalid).unwrap_err().to_string();
            assert!(error.contains("lua_kind_errors"), "must refuse {invalid}: {error}");
        }

        let env = TestEnv::new().await;
        let plugin = env.config_dir().join("plugins/custom_stream.lua");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(
            &plugin,
            r#"
            return {
                name = "custom_stream_test",
                provide = { "custom_stream" },
                contracts = {
                    custom_stream = {
                        methods = {
                            raw_kind = {
                                doc = "raises a provider-shaped kind error table",
                                lua_unchecked = true,
                                lua_kind_errors = true,
                                result = { type = "object" },
                            },
                            raw_without_kind = {
                                doc = "raises a non-kind error table on a lua_kind_errors method",
                                lua_unchecked = true,
                                lua_kind_errors = true,
                                result = { type = "object" },
                            },
                            settled = {
                                doc = "raises a kind table without lua_kind_errors",
                                lua_unchecked = true,
                                result = { type = "object" },
                            },
                        },
                    },
                },
                apply = function(ctx)
                    local cordis = require("cordis")
                    local CustomStream = cordis.class("CustomStream", mare.Service)
                    function CustomStream:constructor(c)
                        CustomStream.super.constructor(self, c, "custom_stream")
                    end
                    function CustomStream:raw_kind()
                        error({ kind = "rate_limit", status = 429, code = "provider_limit" })
                    end
                    function CustomStream:raw_without_kind()
                        error({ code = "unlisted_code", message = "boom" })
                    end
                    function CustomStream:settled()
                        error({ kind = "rate_limit", status = 429, code = "provider_limit" })
                    end
                    CustomStream.new(ctx)
                end,
            }
            "#,
        )
        .unwrap();
        let h = env.host().builtins(&[]).user_plugins().no_session().build().await;
        let svc = h.service_table("custom_stream").expect("custom_stream service loaded");
        let (raw_kind, raw_code, without_kind_code, settled_code): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = h
            .lua()
            .load(
                r#"
                local svc = ...
                local ok1, err1 = pcall(function() return svc:raw_kind() end)
                assert(not ok1 and type(err1) == "table", "raw_kind must raise a table: " .. tostring(err1))
                local ok2, err2 = pcall(function() return svc:raw_without_kind() end)
                assert(not ok2 and type(err2) == "table", "raw_without_kind must raise a table: " .. tostring(err2))
                local ok3, err3 = pcall(function() return svc:settled() end)
                assert(not ok3 and type(err3) == "table", "settled must raise a table: " .. tostring(err3))
                return err1.kind, err1.code, err2.code, err3.code
                "#,
            )
            .call(svc)
            .unwrap();
        assert_eq!(raw_kind.as_deref(), Some("rate_limit"));
        assert_eq!(raw_code.as_deref(), Some("provider_limit"));
        assert_eq!(without_kind_code.as_deref(), Some("internal"));
        assert_eq!(settled_code.as_deref(), Some("internal"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn subagent_service_methods_paginate_and_raise_coded_errors() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["subagent"])
            .faux(json!([{ "text": "alpha-ok" }, { "text": "beta-ok" }]))
            .no_session()
            .build()
            .await;
        expose(&h);
        let summary: String = eval(
            &h,
            r#"
            assert(mare.agent == nil, "mare.agent must be removed without dual path")
            assert(type(mare.subagent) == "table" and type(mare.subagent.start) == "function")
            local svc = C.cordis:get("subagent")
            assert(svc, "subagent service must be published")

            local a = svc:start({ prompt = "one", name = "alpha" })
            assert(type(a.subagent_id) == "string" and a.subagent_id ~= "")
            assert(a.id == nil and a.session_id == nil and a.numericId == nil and a.childSessionId == nil)

            local dup_ok, dup_err = pcall(function() return svc:start({ prompt = "dup", name = "alpha" }) end)
            assert(not dup_ok and dup_err.code == "subagent.duplicate_name", tostring(dup_err))

            local b = svc:start({ prompt = "two", name = "beta" })
            local page1 = svc:list({ limit = 1 })
            assert(#page1.data == 1 and type(page1.next_cursor) == "string", "expected cursor page")
            local page2 = svc:list({ cursor = page1.next_cursor, limit = 1 })
            assert(#page2.data == 1 and page1.data[1].subagent_id ~= page2.data[1].subagent_id)

            local got = svc:get(a.subagent_id)
            assert(got.subagent_id == a.subagent_id and got.name == "alpha")

            local miss_ok, miss_err = pcall(function() return svc:get("missing-subagent-id") end)
            assert(not miss_ok and miss_err.code == "subagent.unknown_subagent", tostring(miss_err))

            local w1 = svc:wait(a.subagent_id, { timeout_ms = 10000 })
            local w2 = svc:wait(b.subagent_id, { timeout_ms = 10000 })
            assert(w1.subagent_id == a.subagent_id and w1.status == "completed" and w1.timed_out == false)
            assert(w2.subagent_id == b.subagent_id and w2.status == "completed")

            local bad_cur_ok, bad_cur_err = pcall(function() return svc:list({ cursor = "1e999" }) end)
            assert(not bad_cur_ok and bad_cur_err.code == "invalid_arguments", tostring(bad_cur_err))
            local neg_ok, neg_err = pcall(function() return svc:wait(a.subagent_id, { timeout_ms = -1 }) end)
            assert(not neg_ok and neg_err.code == "invalid_arguments", tostring(neg_err))
            local steer_ok, steer_err = pcall(function() return svc:steer(a.subagent_id, "too-late") end)
            assert(not steer_ok and steer_err.code == "subagent.steer_rejected", tostring(steer_err))
            local cancel_done = svc:cancel(a.subagent_id)
            assert(next(cancel_done) == nil)
            return a.name .. ":" .. b.name
            "#,
        )
        .await;
        assert_eq!(summary, "alpha:beta");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn subagent_service_cancel_zero_timeout_steer_full_and_identity_by_session_id() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["subagent"])
            .faux(json!([
                { "tool_calls": [{ "name": "slow_hold", "arguments": {} }] },
                { "text": "finished" }
            ]))
            .config(json!({ "agents": { "max_active": 1 } }))
            .build()
            .await;
        expose(&h);
        let ok: String = eval(
            &h,
            r#"
            T:register("slow_hold", {
              description = "Hold child open briefly",
              parameters = { type = "object", properties = {} },
              execute = function(_, tctx)
                mare.task.sleep(3000, tctx.cancel)
                return { content = "ok" }
              end,
            })
            local svc = C.cordis:get("subagent")
            S:create()
            local parent = S:info()
            assert(parent and parent.session_id)
            local child = svc:start({ prompt = "hold", name = "held-worker" })
            local ident = mare.subagent.identity(child.subagent_id)
            assert(ident and ident.subagent_id == child.subagent_id, "identity must accept subagent_id string")

            local z = svc:wait(child.subagent_id, { timeout_ms = 0 })
            assert(z.timed_out == true and z.status == "running", "zero timeout must return running immediately")

            local rej_ok, rej_err = pcall(function() return svc:start({ prompt = "over-cap" }) end)
            assert(not rej_ok and rej_err.code == "subagent.limit_reached", tostring(rej_err))

            for i = 1, 64 do
              local s = svc:steer(child.subagent_id, "s-" .. i)
              assert(next(s) == nil)
            end
            local full_ok, full_err = pcall(function() return svc:steer(child.subagent_id, "overflow") end)
            assert(not full_ok and full_err.code == "subagent.steer_rejected", tostring(full_err))

            local c1 = svc:cancel(child.subagent_id)
            assert(next(c1) == nil)
            local c2 = svc:cancel(child.subagent_id)
            assert(next(c2) == nil)
            local w = svc:wait(child.subagent_id, { timeout_ms = 5000 })
            assert(w.status == "cancelled" or w.status == "completed")
            return "ok"
            "#,
        )
        .await;
        assert_eq!(ok, "ok");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn subagent_start_reports_distinct_refusals() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(json!([
                { "tool_calls": [{ "id": "c1", "name": "slow_hold", "arguments": {} }] },
                { "text": "done" }
            ]))
            .config(json!({ "agents": { "max_active": 1 } }))
            .build()
            .await;
        expose(&h);
        let (invalid_code, limit_code, closed_code, invalid_while_closed_code): (String, String, String, String) =
            eval(
                &h,
                r#"
            T:register("slow_hold", {
              description = "Hold child open briefly",
              parameters = { type = "object", properties = {} },
              execute = function(_, tctx)
                mare.task.sleep(3000, tctx.cancel)
                return { content = "ok" }
              end,
            })
            local svc = C.cordis:get("subagent")
            S:create()

            -- 1. Invalid options must fail with `invalid_arguments`.
            local bad_ok, bad_err = pcall(svc.start, svc, { prompt = "x", fork = 0 })
            assert(not bad_ok and type(bad_err) == "table", tostring(bad_err))

            -- 2. Exceeding capacity limit must fail with `subagent.limit_reached`.
            local child = svc:start({ prompt = "hold", name = "holder" })
            local lim_ok, lim_err = pcall(svc.start, svc, { prompt = "over-cap" })
            assert(not lim_ok and type(lim_err) == "table", tostring(lim_err))

            -- 3. Starting while the session's agent tree is draining/closed must fail with `subagent.tree_closed`.
            local drain_entered_tx, drain_entered_rx = mare.task.channel()
            local drain_done_tx, drain_done_rx = mare.task.channel()
            mare.task.spawn(function()
              drain_entered_tx:send(true)
              svc:drain()
              drain_done_tx:send(true)
            end)
            assert(drain_entered_rx:recv())
            local closed_ok, closed_err = pcall(svc.start, svc, { prompt = "while-draining" })
            assert(not closed_ok and type(closed_err) == "table", tostring(closed_err))

            -- 4. Argument validation still takes precedence over `subagent.tree_closed`.
            local bad_closed_ok, bad_closed_err = pcall(svc.start, svc, { prompt = "x", fork = 0 })
            assert(not bad_closed_ok and type(bad_closed_err) == "table", tostring(bad_closed_err))

            pcall(svc.cancel, svc, child.subagent_id)
            assert(drain_done_rx:recv())
            return bad_err.code, lim_err.code, closed_err.code, bad_closed_err.code
            "#,
            )
            .await;
        assert_eq!(invalid_code, "invalid_arguments");
        assert_eq!(limit_code, "subagent.limit_reached");
        assert_eq!(closed_code, "subagent.tree_closed");
        assert_eq!(invalid_while_closed_code, "invalid_arguments");
        h.shutdown().await;
    })
    .await;
}

/// (a) A rejected `/fork` (no completed turn yet) must NOT drain/kill running background
/// subagents; (b) if `drain_session` times out, the session's agent tree must still be replaced
/// with a usable scope so subsequent `mare.subagent.start` calls in the current session succeed.
#[tokio::test]
async fn drain_timeout_replaces_tree_and_rejected_fork_preserves_running_subagent() {
    local(async {
        let env = TestEnv::new().await;
        let entered_flag = env.cwd.join("stubborn-entered.flag");
        let plugins_dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("stubborn-tool.lua"),
            format!(
                r#"return {{
                name = "stubborn-tool",
                inject = {{ "tools" }},
                apply = function(ctx)
                    ctx.tools:register("stubborn_hold", {{
                        description = "ignores cancel for 1.5s",
                        parameters = {{ type = "object", properties = {{}} }},
                        execute = function()
                            mare.fs.write({flag:?}, "entered")
                            -- Deliberately sleep without cancel token to outlive short shutdown_grace.
                            mare.task.sleep(1500)
                            return {{ content = "held" }}
                        end,
                    }})
                end,
            }}"#,
                flag = entered_flag.to_string_lossy()
            ),
        )
        .unwrap();

        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .user_plugins()
            .faux(json!([
                { "tool_calls": [{ "id": "tc-stubborn", "name": "stubborn_hold", "arguments": {} }] },
                { "text": "done-1" },
                { "text": "done-2" }
            ]))
            .config(json!({
                "shutdown_grace_ms": 150,
                "plugins": { "permissions": { "enabled": false } }
            }))
            .build()
            .await;
        expose(&h);
        exec(&h, "SUB = C.cordis:get('subagent'); S:create()").await;

        // Start a background subagent running `stubborn_hold` (no completed parent turn yet).
        let sub_id: String = eval(
            &h,
            r#"local r = SUB:start({ prompt = "hold", tools = { "stubborn_hold" } })
               return r.subagent_id"#,
        )
        .await;

        // Wait until the child actually enters `stubborn_hold` so drain hits the stubborn tool.
        wait_until(10, "subagent entered stubborn_hold", || {
            let flag = entered_flag.clone();
            async move { flag.exists() }
        })
        .await;

        // 1. Rejected fork (no recorded completed turn) must NOT cancel the running subagent.
        let (fork_ok, status_after_rejected_fork): (bool, String) = eval(
            &h,
            &format!(
                r#"local ok = pcall(function() return S:fork({{}}) end)
                   mare.task.sleep(50)
                   local st = (SUB:get({sub_id:?}) or {{}}).status or ""
                   return ok, st"#
            ),
        )
        .await;
        assert!(!fork_ok, "fork without a completed turn must be rejected");
        assert_eq!(
            status_after_rejected_fork, "running",
            "rejected fork must NOT kill running background subagents"
        );

        // 2. Drain times out (shutdown_grace_ms = 200ms < 1500ms stubborn tool), leaving user in
        //    the current session; subsequent `SUB:start` MUST succeed (not fail with tree_closed).
        let (drain_ok, next_start_ok, next_start_err): (bool, bool, String) = eval(
            &h,
            r#"local dok = pcall(mare.subagent.drain_session)
               local sok, sres = pcall(function() return SUB:start({ prompt = "after-timeout" }) end)
               return dok, sok, sok and "" or tostring(sres)"#,
        )
        .await;
        assert!(!drain_ok, "drain_session should time out on stubborn subagent");
        assert!(
            next_start_ok,
            "after drain_session timeout, starting a new subagent in the current session must NOT fail with tree_closed: {next_start_err}"
        );
        h.shutdown().await;
    })
    .await;
}

/// When the `subagent` tool with `wait = true` is interrupted by cancel while the subagent
/// keeps running in the background, it must still persist `"start"`/`"end"` records and deliver
/// the background completion notification.
#[tokio::test]
async fn subagent_wait_true_interrupted_persists_start_and_delivers_completion_notice() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(json!([
                { "text": "bg-finished-ok", "delay_ms": 800 }
            ]))
            .config(json!({
                "plugins": { "permissions": { "enabled": false } }
            }))
            .build()
            .await;
        expose(&h);

        let (sub_id, saw_start_rec, saw_end_rec, saw_notice): (String, bool, bool, bool) = eval(
            &h,
            r#"S:create()
               local tok = mare.task.token()
               local notices = {}
               local notice_tx, notice_rx = mare.task.channel()
               local orig_notify = A.notify
               A.notify = function(self, msg, opts)
                 notices[#notices + 1] = tostring(msg)
                 local r = orig_notify(self, msg, opts)
                 notice_tx:send(true)
                 return r
               end
               local orig_wait = mare.subagent.wait
               mare.subagent.wait = function(id, opts)
                 mare.subagent.wait = orig_wait
                 mare.task.spawn(function() tok:cancel() end)
                 return orig_wait(id, opts)
               end
               local sub_tool = assert(T:get("subagent"), "subagent tool registered")
               local res = sub_tool.execute(
                 { prompt = "finish later", description = "slow-worker", wait = true },
                 { cancel = tok }
               )
               assert(res.details and res.details.cancelled == true, "expected interrupted wait: " .. mare.json.encode(res))
               local sid = res.details.subagent_id or ""
               assert(sid ~= "", "expected subagent_id in interrupted wait details: " .. tostring(res.content))
               -- Wait for the background watcher to persist 'end' and deliver the completion notice.
               local notified = mare.task.timeout(15000, function() notice_rx:recv() end)
               assert(notified, "timed out waiting for subagent completion notice")
               local recs = S:list_records("subagent").data or {}
               local has_start, has_end = false, false
               for _, d in ipairs(recs) do
                 if type(d) == "table" and d.subagent_id == sid and d.event == "start" then has_start = true end
                 if type(d) == "table" and d.subagent_id == sid and d.event == "end" then has_end = true end
               end
               local has_notice = false
               for _, n in ipairs(notices) do
                 if n:find(sid, 1, true) and n:find("finished", 1, true) then has_notice = true end
               end
               return sid, has_start, has_end, has_notice"#,
        )
        .await;
        assert!(!sub_id.is_empty());
        assert!(saw_start_rec, "interrupted wait=true subagent must persist 'start' record");
        assert!(saw_end_rec, "interrupted wait=true subagent must persist 'end' record upon completion");
        assert!(saw_notice, "interrupted wait=true subagent must deliver completion notification");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn repro_236_explicitly_allowed_deferred_tool_callable_without_tool_search() {
    local(async {
        let env = TestEnv::new().await;
        let plugins_dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let req_capture = env.home.path().join("child-req.json");
        std::fs::write(
            plugins_dir.join("native-tu-provider.lua"),
            format!(
                r#"return {{
    name = "native-tu-provider",
    inject = {{ "llm", "tools" }},
    apply = function(ctx)
        ctx.llm:register_provider({{
            provider = "nat",
            no_key = true,
            models = {{
                {{ model_id = "tu", name = "TU", reasoning = false, tool_update = "addition_only" }},
            }},
            stream = function(req)
                local wire = require("provider-anthropic.wire")
                local converted = wire.convert_tools(req.tools)
                mare.fs.write({:?}, mare.json.encode({{
                    converted = mare.json.array(converted),
                    raw = mare.json.array(req.tools or {{}}),
                    model_tu = req.model and req.model.tool_update,
                }}))
                return {{ role = "assistant", content = {{ {{ type = "text", text = "done" }} }}, stop_reason = "stop" }}
            end,
        }})
        ctx.tools:register("mcp__srv__ping", {{
            description = "deferred mcp ping tool",
            exposure = "deferred",
            side_effect = "read",
            parameters = {{ type = "object", properties = {{}}, additionalProperties = false }},
            execute = function() return {{ content = "pong" }} end,
        }})
    end,
}}"#,
                req_capture.to_string_lossy()
            ),
        )
        .unwrap();

        let mut b = env
            .host()
            .builtins(&["subagent", "provider-anthropic"])
            .config(json!({ "model": "nat/tu" }))
            .no_session();
        b.opts.project_trusted = true;
        b.opts.load_init = true;
        b.opts.no_user_plugins = false;
        let h = b.build().await;
        expose(&h);

        let _res: String = eval(
            &h,
            r#"
            assert(C.cordis:get("llm"):set_model("nat/tu"))
            local svc = C.cordis:get("subagent")
            local run = svc:start({
                prompt = "ping",
                model = "nat/tu",
                tools = { "mcp__srv__ping" },
            })
            local done = svc:wait(run.subagent_id, { timeout_ms = 15000 })
            assert(done and done.status == "completed", mare.json.encode(done))
            return tostring(done.result or "")
            "#,
        )
        .await;

        let raw = std::fs::read_to_string(&req_capture).expect("captured child wire tools");
        let captured: Value = serde_json::from_str(&raw).unwrap();
        let tools = captured["converted"].as_array().unwrap_or_else(|| panic!("captured={captured}"));
        let ping = tools
            .iter()
            .find(|t| t["name"] == "mcp__srv__ping")
            .unwrap_or_else(|| panic!("expected mcp__srv__ping in child request tools: {captured}"));
        let has_tool_search = tools.iter().any(|t| t["name"] == "tool_search");
        assert!(!has_tool_search, "fixture must omit tool_search: {captured}");
        assert_ne!(
            ping.get("defer_loading").and_then(Value::as_bool),
            Some(true),
            "explicitly allowed deferred tool was still sent with defer_loading=true without tool_search: {captured}"
        );

        // With `tool_search` present on the same native tool-update model, `defer_loading=true` is preserved.
        let _res2: String = eval(
            &h,
            r#"
            local svc = C.cordis:get("subagent")
            local run = svc:start({
                prompt = "ping with search",
                model = "nat/tu",
                tools = { "tool_search", "mcp__srv__ping" },
            })
            local done = svc:wait(run.subagent_id, { timeout_ms = 15000 })
            assert(done and done.status == "completed", mare.json.encode(done))
            return tostring(done.result or "")
            "#,
        )
        .await;
        let raw2 = std::fs::read_to_string(&req_capture).expect("captured child wire tools with tool_search");
        let captured2: Value = serde_json::from_str(&raw2).unwrap();
        let tools2 = captured2["converted"].as_array().unwrap_or_else(|| panic!("captured2={captured2}"));
        assert!(tools2.iter().any(|t| t["name"] == "tool_search"), "expected tool_search: {captured2}");
        let ping2 = tools2
            .iter()
            .find(|t| t["name"] == "mcp__srv__ping")
            .unwrap_or_else(|| panic!("expected mcp__srv__ping in child request tools: {captured2}"));
        assert_eq!(
            ping2.get("defer_loading").and_then(Value::as_bool),
            Some(true),
            "deferred tool must stay defer_loading=true when tool_search is present: {captured2}"
        );

        // Verify non-native and implicit-allowlist cases directly on `tools:definitions` + `tools:bind_declarations`:
        // 1. Non-native model with `tools_enabled = { "tool_search", "mcp__srv__ping" }` (no `call_tool`):
        //    `tool_search` alone cannot execute deferred tools on a non-native model, so the explicitly allowed
        //    deferred tool must be declared eagerly.
        // 2. Non-native model with `tools_enabled = { "tool_search", "call_tool", "mcp__srv__ping" }`:
        //    both loader tools are available, so the deferred tool stays omitted (deferred).
        // 3. No explicit allowlist (`tools_enabled = nil`), disabling both `tool_search` and `call_tool`:
        //    must NOT promote deferred tools to eager on either non-native or native models.
        let (
            non_native_search_only_eager,
            non_native_both_deferred,
            non_native_disabled_wrappers_not_eager,
            native_disabled_search_not_eager,
        ): (bool, bool, bool, bool) = eval(
            &h,
            r#"
            local non_native = { id = "plain", provider = "nat" }
            local native = { id = "tu", provider = "nat", tool_update = "addition_only" }
            local function find_decl(list, name)
                for _, d in ipairs(list) do
                    if d.name == name then return d end
                end
                return nil
            end

            local orig_enabled = mare.config.tools_enabled
            mare.config.tools_enabled = { "tool_search", "mcp__srv__ping" }
            local d1 = T:definitions(non_native)
            local _, w1 = T:bind_declarations(d1, { model = non_native })
            local case1 = find_decl(d1, "mcp__srv__ping") ~= nil
                and find_decl(w1, "mcp__srv__ping") ~= nil
                and find_decl(w1, "mcp__srv__ping").defer_loading == nil

            mare.config.tools_enabled = { "tool_search", "call_tool", "mcp__srv__ping" }
            local d2 = T:definitions(non_native)
            local case2 = find_decl(d2, "mcp__srv__ping") == nil

            mare.config.tools_enabled = orig_enabled
            T:disable("tool_search")
            T:disable("call_tool")
            local d3 = T:definitions(non_native)
            local case3 = find_decl(d3, "mcp__srv__ping") == nil
            local d4 = T:definitions(native)
            local _, w4 = T:bind_declarations(d4, { model = native })
            local p4 = find_decl(w4, "mcp__srv__ping")
            local case4 = p4 ~= nil and p4.defer_loading == true
            T:enable("tool_search")
            T:enable("call_tool")
            return case1, case2, case3, case4
            "#,
        )
        .await;
        assert!(
            non_native_search_only_eager,
            "non-native model with tools_enabled={{tool_search, target}} and no call_tool must expose target eagerly"
        );
        assert!(
            non_native_both_deferred,
            "non-native model with both tool_search and call_tool must keep deferred target deferred"
        );
        assert!(
            non_native_disabled_wrappers_not_eager,
            "disabling tool_search+call_tool without an explicit allowlist must not promote deferred tools to eager on non-native models"
        );
        assert!(
            native_disabled_search_not_eager,
            "disabling tool_search without an explicit allowlist must not promote deferred tools to eager on native models"
        );
        h.shutdown().await;
    })
    .await;
}
