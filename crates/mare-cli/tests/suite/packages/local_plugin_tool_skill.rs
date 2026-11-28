//! End-to-end faux test driving the bundled `plugin-authoring` skill across all 5 steps:
//! 1. read the bundled skill and write a local plugin into an approved local dev package;
//! 2. register a tool via `ctx.tools:register`;
//! 3. load it and call it on the next model request (v1);
//! 4. edit the plugin, run `/reload <name>`, and call it on the next request (v2);
//! 5. break the plugin on purpose, see the load error, and confirm the previous version (v2) still works.

use super::*;

#[tokio::test]
async fn bundled_plugin_authoring_skill_drives_write_load_call_reload_and_rollback_e2e() {
    local(async {
        let mut env = TestEnv::new().await;
        // Do NOT pre-extract runtime assets manually: the first skills discovery / system prompt
        // build must materialize bundled runtime assets on demand without racing startup.
        let rt_dir = mare_plugin::runtime::dir();
        let bundled_skill_path = rt_dir.join("skills/plugin-authoring/SKILL.md");
        assert!(
            !bundled_skill_path.exists(),
            "test must start before runtime extraction: {}",
            bundled_skill_path.display()
        );

        // Approved local dev package directory declared via public `mare.plugins.add { { dir = ... } }`.
        let dev_pkg = env.cwd.join("my-local-tools");
        std::fs::create_dir_all(dev_pkg.join("plugins")).unwrap();
        let plugin_file = dev_pkg.join("plugins/local_tagger.lua");

        let plugin_v1 = r#"return {
  name = "local_tagger",
  inject = { "tools" },
  apply = function(ctx)
    ctx.tools:register("tag_text", {
      snippet = "Tag text with a versioned prefix",
      description = "Return the input text prefixed with a tag.",
      parameters = {
        type = "object",
        properties = { text = { type = "string", description = "Text to tag" } },
        required = { "text" },
      },
      side_effect = "read",
      execute = function(args)
        return { content = "v1:" .. tostring(args.text or "") }
      end,
    })
  end,
}
"#;
        let plugin_broken = "return { name = 'local_tagger', broken syntax !!!\n";

        // Phase 1 script (before full `/reload`): model reads the bundled skill and writes the plugin.
        let phase1_script = json!([
            {
                "tool_calls": [{
                    "name": "read",
                    "arguments": { "path": bundled_skill_path.to_string_lossy() }
                }]
            },
            {
                "tool_calls": [{
                    "name": "write",
                    "arguments": {
                        "path": plugin_file.to_string_lossy(),
                        "content": plugin_v1
                    }
                }]
            },
            { "text": "WROTE_V1_PLUGIN" }
        ]);

        // Phase 2 script (after full `/reload` loads `local_tagger`; subsequent `/reload local_tagger`
        // reloads only `local_tagger`, keeping `provider-faux` advancing across steps 3 -> 4 -> 5).
        let phase2_script = json!([
            // Step 3: call `tag_text` (v1) on the next request.
            {
                "tool_calls": [{
                    "name": "tag_text",
                    "arguments": { "text": "hello" }
                }]
            },
            { "text": "CALLED_V1_OK" },

            // Step 4a: read `local_tagger.lua` (read-before-edit) then edit it to v2.
            {
                "tool_calls": [{
                    "name": "read",
                    "arguments": { "path": plugin_file.to_string_lossy() }
                }]
            },
            {
                "tool_calls": [{
                    "name": "edit",
                    "arguments": {
                        "path": plugin_file.to_string_lossy(),
                        "old_string": "return { content = \"v1:\" .. tostring(args.text or \"\") }",
                        "new_string": "return { content = \"v2:\" .. tostring(args.text or \"\") }"
                    }
                }]
            },
            { "text": "EDITED_TO_V2" },

            // Step 4b (after `/reload local_tagger`): call `tag_text` (v2).
            {
                "tool_calls": [{
                    "name": "tag_text",
                    "arguments": { "text": "hello" }
                }]
            },
            { "text": "CALLED_V2_OK" },

            // Step 5a: break `local_tagger.lua` on purpose.
            {
                "tool_calls": [{
                    "name": "write",
                    "arguments": {
                        "path": plugin_file.to_string_lossy(),
                        "content": plugin_broken
                    }
                }]
            },
            { "text": "BROKE_PLUGIN" },

            // Step 5b (after failed `/reload local_tagger`): call `tag_text` and confirm v2 still works.
            {
                "tool_calls": [{
                    "name": "tag_text",
                    "arguments": { "text": "still-alive" }
                }]
            },
            { "text": "PREVIOUS_V2_STILL_WORKS" }
        ]);

        let script_path = env.home.path().join("faux-script.json");
        std::fs::write(&script_path, phase1_script.to_string()).unwrap();
        env.set_var("MARE_FAUX_SCRIPT", script_path.to_str().unwrap());

        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ model = 'faux/echo' }}\nmare.plugins.add {{ {{ dir = {dir:?} }} }}\n",
                dir = dev_pkg.to_string_lossy(),
            ),
        )
        .unwrap();

        let h = env.host().user_config(true).builtins(&["plugins"]).no_session().build().await;
        expose(&h);

        // Verify the bundled `plugin-authoring` skill is discovered and advertised in `<available_skills>`.
        let (has_skill, skill_loc, system_prompt): (bool, String, String) = eval(
            &h,
            r#"
            local X = C.cordis:get("context")
            local L = C.cordis:get("llm")
            local loc = ""
            for _, sk in ipairs(X:skills()) do
                if sk.name == "plugin-authoring" then
                    loc = sk.path
                end
            end
            return loc ~= "", loc, X:system_prompt(L:current())
            "#,
        )
        .await;
        assert!(has_skill, "context:skills() must include bundled plugin-authoring skill");
        assert!(
            skill_loc.replace('\\', "/").ends_with("runtime/skills/plugin-authoring/SKILL.md"),
            "unexpected bundled skill path: {skill_loc}"
        );
        assert!(
            system_prompt.contains("<name>plugin-authoring</name>") && system_prompt.contains(&skill_loc),
            "system prompt <available_skills> must advertise plugin-authoring:\n{system_prompt}"
        );

        let attach_observers = r#"
            OBSERVED_RESULTS = {}
            OFFERED_TOOLS = {}
            C.cordis:on("request/build", function(req, next)
                local names = {}
                for _, t in ipairs(req.tools or {}) do
                    names[t.name] = true
                end
                OFFERED_TOOLS[#OFFERED_TOOLS + 1] = names
                return next()
            end)
            C.cordis:on("tool/after", function(data, ev)
                local name = data.call and data.call.name or ""
                local content = type(data.result) == "table" and tostring(data.result.content or "") or tostring(data.result or "")
                OBSERVED_RESULTS[#OBSERVED_RESULTS + 1] = { name = name, content = content, is_error = data.result and data.result.is_error == true }
                return ev.next(data)
            end)
        "#;
        exec(&h, attach_observers).await;

        // Steps 1 & 2: Read bundled skill text and write plugin into the approved local dev package.
        let r1 = h
            .prompt("Use the plugin-authoring skill to create a local_tagger plugin registering tag_text.", json!({}))
            .await
            .unwrap();
        assert!(r1["error"].is_null(), "prompt 1 failed: {r1}");
        assert!(plugin_file.is_file(), "model did not write plugin file");

        let skill_read_ok: bool = eval(
            &h,
            r#"
            local first = OBSERVED_RESULTS[1]
            return first ~= nil
                and first.name == "read"
                and not first.is_error
                and first.content:find("mare.plugins.add", 1, true) ~= nil
                and first.content:find("ctx.tools:register", 1, true) ~= nil
                and first.content:find("/reload", 1, true) ~= nil
                and first.content:find("previous working version", 1, true) ~= nil
            "#,
        )
        .await;
        assert!(skill_read_ok, "bundled SKILL.md read must contain the 5-step public authoring flow");

        // Switch faux script to Phase 2 before full `/reload` (`mare.reload()`), as instructed in step 3.
        std::fs::write(&script_path, phase2_script.to_string()).unwrap();
        exec(&h, "mare.reload()").await;
        exec(&h, attach_observers).await;

        let active_v1: bool = eval(&h, "return mare.plugins.status('local_tagger').active == true").await;
        assert!(active_v1, "local_tagger plugin must be active after /reload");

        // Step 3: Call `tag_text` on the next model request and verify it was offered and returns v1.
        let r2 = h.prompt("Call tag_text with hello.", json!({})).await.unwrap();
        assert!(r2["error"].is_null(), "prompt 2 failed: {r2}");
        let (offered_v1, v1_result): (bool, String) = eval(
            &h,
            r#"
            local offered = OFFERED_TOOLS[1] ~= nil and OFFERED_TOOLS[1]["tag_text"] == true
            for i = #OBSERVED_RESULTS, 1, -1 do
                if OBSERVED_RESULTS[i].name == "tag_text" then
                    return offered, OBSERVED_RESULTS[i].content
                end
            end
            return offered, ""
            "#,
        )
        .await;
        assert!(offered_v1, "newly registered tag_text tool must be offered on the next model request");
        assert_eq!(v1_result, "v1:hello", "step 3 must call newly registered v1 tool");

        // Step 4: Edit plugin to v2, run `/reload local_tagger`, and call on next request.
        let r3 = h.prompt("Update local_tagger to prefix with v2:.", json!({})).await.unwrap();
        assert!(r3["error"].is_null(), "prompt 3 failed: {r3}");
        assert!(
            std::fs::read_to_string(&plugin_file).unwrap().contains("v2:"),
            "step 4 edit must update plugin file on disk"
        );
        let reloaded_v2: (bool, String) = eval(
            &h,
            r#"
            local ok, res = pcall(function() return C.cordis:get("kernel"):reload("local_tagger") end)
            return ok, tostring(res)
            "#,
        )
        .await;
        assert!(reloaded_v2.0, "/reload local_tagger failed for v2: {}", reloaded_v2.1);

        let r4 = h.prompt("Call tag_text with hello again.", json!({})).await.unwrap();
        assert!(r4["error"].is_null(), "prompt 4 failed: {r4}");
        let v2_result: String = eval(
            &h,
            r#"
            for i = #OBSERVED_RESULTS, 1, -1 do
                if OBSERVED_RESULTS[i].name == "tag_text" then
                    return OBSERVED_RESULTS[i].content
                end
            end
            return ""
            "#,
        )
        .await;
        assert_eq!(v2_result, "v2:hello", "step 4 must serve updated v2 tool after /reload local_tagger");

        // Step 5: Break plugin on purpose, run `/reload local_tagger`, observe load error,
        // and confirm the previous working v2 tool still executes on the next request.
        let r5 = h.prompt("Break local_tagger.lua on purpose.", json!({})).await.unwrap();
        assert!(r5["error"].is_null(), "prompt 5 failed: {r5}");
        assert!(
            std::fs::read_to_string(&plugin_file).unwrap().contains("broken syntax"),
            "step 5 write must write broken Lua source to disk"
        );
        let (broken_ok, broken_err, problems): (bool, String, String) = eval(
            &h,
            r#"
            local K = C.cordis:get("kernel")
            local ok, err = pcall(function() return K:reload("local_tagger") end)
            return ok, tostring(err), table.concat(K:problems(), "\n")
            "#,
        )
        .await;
        assert!(
            !broken_ok || problems.contains("local_tagger"),
            "broken reload must surface a load error (ok={broken_ok}, err={broken_err}, problems={problems})"
        );
        let combined_err = format!("{broken_err}\n{problems}");
        assert!(
            combined_err.contains("local_tagger")
                || combined_err.contains("syntax")
                || combined_err.contains("expected"),
            "expected syntax/load error details: {combined_err}"
        );

        let r6 = h.prompt("Call tag_text with still-alive to confirm rollback.", json!({})).await.unwrap();
        assert!(r6["error"].is_null(), "prompt 6 failed after broken reload: {r6}");
        let after_break_result: String = eval(
            &h,
            r#"
            for i = #OBSERVED_RESULTS, 1, -1 do
                if OBSERVED_RESULTS[i].name == "tag_text" then
                    return OBSERVED_RESULTS[i].content
                end
            end
            return ""
            "#,
        )
        .await;
        assert_eq!(
            after_break_result, "v2:still-alive",
            "step 5 must keep previous working v2 tool active after a failed reload"
        );

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn user_skill_shadows_bundled_skill_and_runtime_ensure_prunes_stale_skills() {
    local(async {
        let env = TestEnv::new().await;
        let rt_dir = mare_plugin::runtime::ensure().unwrap();
        assert!(rt_dir.join("skills/plugin-authoring/SKILL.md").is_file());

        // Plant a stale bundled skill and roll VERSION back so `ensure()` refreshes and prunes it.
        let stale = rt_dir.join("skills/stale-skill/SKILL.md");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, "---\nname: stale-skill\ndescription: stale\n---\n").unwrap();
        std::fs::write(rt_dir.join("VERSION"), "0.0.0-old").unwrap();
        mare_plugin::runtime::ensure().unwrap();
        assert!(!stale.exists(), "runtime::ensure() must prune stale bundled skills on version change");
        assert!(rt_dir.join("skills/plugin-authoring/SKILL.md").is_file());

        // A user skill in configured `skill_dirs` with the same name must shadow the bundled skill.
        let user_skills_root = env.config_dir().join("skills");
        let user_skill = user_skills_root.join("plugin-authoring/SKILL.md");
        std::fs::create_dir_all(user_skill.parent().unwrap()).unwrap();
        std::fs::write(
            &user_skill,
            "---\nname: plugin-authoring\ndescription: USER OVERRIDE AUTHORING SKILL\n---\nuser override body\n",
        )
        .unwrap();

        let h = env
            .host()
            .config(json!({ "plugins": { "context": { "skill_dirs": [user_skills_root] } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        let desc: String = eval(
            &h,
            r#"
            for _, sk in ipairs(C.cordis:get("context"):skills()) do
                if sk.name == "plugin-authoring" then return sk.description end
            end
            return ""
            "#,
        )
        .await;
        assert_eq!(desc, "USER OVERRIDE AUTHORING SKILL");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn bundled_skill_refreshes_same_version_stale_runtime_and_waits_for_extract_lock() {
    local(async {
        // 1. Pre-existing same-version runtime dir (VERSION matches CARGO_PKG_VERSION, but written
        // before bundled skills / asset fingerprint existed): first skills discovery must refresh it.
        {
            let env = TestEnv::new().await;
            let rt_dir = mare_plugin::runtime::dir();
            std::fs::create_dir_all(&rt_dir).unwrap();
            std::fs::write(rt_dir.join("VERSION"), env!("CARGO_PKG_VERSION")).unwrap();
            assert!(!rt_dir.join("skills/plugin-authoring/SKILL.md").exists());

            let h = env.host().no_session().build().await;
            expose(&h);
            let skill_path: String = eval(
                &h,
                r#"
                for _, sk in ipairs(C.cordis:get("context"):skills()) do
                    if sk.name == "plugin-authoring" then return sk.path end
                end
                return ""
                "#,
            )
            .await;
            assert!(
                skill_path.replace('\\', "/").ends_with("runtime/skills/plugin-authoring/SKILL.md"),
                "same-version stale runtime dir must refresh bundled skills on first discovery; got {skill_path:?}"
            );
            assert!(rt_dir.join("skills/plugin-authoring/SKILL.md").is_file());
            h.shutdown().await;
        }

        // 2. Startup race: simulate a background extraction currently holding `runtime.lock` on a
        // fresh runtime dir; first skills discovery must wait for the lock and observe the skill.
        {
            let env2 = TestEnv::new().await;
            let rt_dir2 = mare_plugin::runtime::dir();
            let lock_path = rt_dir2.with_file_name("runtime.lock");
            let guard = mare_shared::private::lock_file_blocking(
                &lock_path,
                Some(std::time::Duration::from_secs(5)),
                std::time::Duration::from_millis(10),
                true,
                None,
            )
            .unwrap();
            let h2 = env2.host().no_session().build().await;
            expose(&h2);

            let releaser = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(150));
                drop(guard);
            });
            let skill_path2: String = eval(
                &h2,
                r#"
                for _, sk in ipairs(C.cordis:get("context"):skills()) do
                    if sk.name == "plugin-authoring" then return sk.path end
                end
                return ""
                "#,
            )
            .await;
            releaser.join().unwrap();
            assert!(
                skill_path2.replace('\\', "/").ends_with("runtime/skills/plugin-authoring/SKILL.md"),
                "first skills discovery must wait for runtime.lock instead of scanning an empty dir; got {skill_path2:?}"
            );
            h2.shutdown().await;
        }
    })
    .await;
}
