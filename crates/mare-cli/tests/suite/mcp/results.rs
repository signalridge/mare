//! Tool results: resources, prompts, instructions, shaping and spill failures.

use super::*;

/// Resources through the three resource tools, a prompt as `/mcp__<server>__<prompt>`, the
/// server's instructions as its system-prompt line, and result shaping: a long text is cut in
/// the middle with the full text saved to a file, a resource link shows name and address, audio
/// gets a note, structured content is passed along with the text.
#[tokio::test]
async fn mcp_resources_prompts_instructions_and_result_shaping() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "rich");
        let entry = with(
            fake.entry(json!({ "rich": true, "tools": ["echo", "big"], "instructions": "Docs search.\nMore detail." })),
            json!({ "tier": "direct" }),
        );
        let h = env.host().builtins(&["mcp"]).config(servers(json!({ "fake": entry }))).no_session().build().await;
        expose(&h);
        wait_until(15, "the tools", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__big') ~= nil").await }
        })
        .await;
        let (list, read): (String, String) = eval(
            &h,
            r#"local l = T:execute({ id = "r1", name = "list_mcp_resources", arguments = {} }, {})
               local r = T:execute({ id = "r2", name = "read_mcp_resource", arguments = { server = "fake", uri = "mem://a" } }, {})
               local function text(x) return type(x.content) == "table" and x.content[1].text or tostring(x.content) end
               return text(l), text(r)"#,
        )
        .await;
        assert!(list.contains("mem://a") && list.contains("\"server\":\"fake\""), "{list}");
        assert_eq!(read, "body of mem://a");
        let texts: Vec<String> = eval(
            &h,
            r#"local r = T:execute({ id = "b1", name = "mcp__fake__big", arguments = {} }, {})
               local out = {} for _, c in ipairs(r.content) do out[#out + 1] = c.text or c.type end return out"#,
        )
        .await;
        assert!(texts[0].len() < 25_000 && texts[0].contains("chars truncated") && !texts[0].contains("MIDDLE"), "cut");
        let path = texts[0].split("the full result is in ").nth(1).unwrap().split('…').next().unwrap();
        assert!(std::fs::read_to_string(path).unwrap().contains("MIDDLE"), "full text saved");
        assert!(texts[1].contains("mem://a") && texts[1].contains("read_mcp_resource"), "{}", texts[1]);
        assert!(texts[2].contains("audio") && !texts[2].contains("AAAA"), "{}", texts[2]);
        assert!(texts[3].contains("{\"n\":1}"), "{}", texts[3]);
        let system: String = eval(&h, "return C.cordis:get('context'):system_prompt()").await;
        assert!(system.contains("- fake: Docs search. (tools already loaded)"), "{system}");
        let submitted: String = eval(
            &h,
            r#"local got
               C.cordis:on("input/submit", function(ev) got = ev.text end)
               C.cordis:get("commands"):run("/mcp__fake__greet world")
               return got"#,
        )
        .await;
        assert_eq!(submitted, "Hello world");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mcp_large_result_spill_failure_is_a_tool_error() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "spill-failure");
        let entry = with(fake.entry(json!({ "rich": true, "tools": ["big"] })), json!({ "tier": "direct" }));
        let h = env.host().builtins(&["mcp"]).config(servers(json!({ "fake": entry }))).no_session().build().await;
        expose(&h);
        wait_until(15, "the tool", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__big') ~= nil").await }
        })
        .await;
        let (is_error, output): (bool, String) = eval(
            &h,
            r#"mare.mcp.spill = function() error("disk full") end
               local r = T:execute({ id = "b1", name = "mcp__fake__big", arguments = {} }, {})
               return r.is_error == true, mare.json.encode(r.content)"#,
        )
        .await;
        assert!(is_error && output.contains("could not be saved") && output.contains("disk full"), "{output}");
        assert!(!output.contains("chars truncated"), "a partial result must not appear successful: {output}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mcp_resource_errors_and_prompt_unregistration() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "rich");
        let entry = with(fake.entry(json!({ "rich": true, "tools": ["echo"] })), json!({ "tier": "direct" }));
        let broken = json!({ "command": "/nonexistent/mcp-binary-that-fails", "tier": "direct" });
        let h = env.host().builtins(&["mcp"]).config(servers(json!({ "fake": entry, "broken": broken }))).no_session().build().await;
        expose(&h);
        wait_until(15, "fake ready", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__echo') ~= nil").await }
        })
        .await;

        let (list_err, list_msg, tmpl_err, has_cmd_before, has_cmd_after): (bool, String, bool, bool, bool) = eval(
            &h,
            r#"
            local l = T:execute({ id = "r1", name = "list_mcp_resources", arguments = { server = "broken" } }, {})
            local lt = T:execute({ id = "r2", name = "list_mcp_resource_templates", arguments = { server = "broken" } }, {})
            local cmd = C.cordis:get("commands")
            local before = cmd:get("mcp__fake__greet") ~= nil
            C.cordis:get("commands"):run("/mcp disable fake")
            local after = cmd:get("mcp__fake__greet") ~= nil
            return l.is_error == true, tostring(l.content), lt.is_error == true, before, after
            "#,
        )
        .await;
        assert!(list_err, "explicit broken server must error instead of returning empty: {list_msg}");
        assert!(tmpl_err, "explicit broken server templates must error");
        assert!(has_cmd_before, "prompt command should be registered while connected");
        assert!(!has_cmd_after, "prompt command must be unregistered when server is disabled");
        h.shutdown().await;
    })
    .await
}

/// Real `notifications/progress` from an MCP server reaches both `client:call(..., { progress })`
/// (matching the LuaLS `mare.McpCallOpts.progress` contract) and `t_ctx.update` on tool execution.
/// Scenario ref: reference implementation.
#[tokio::test]
async fn mcp_progress_notification_forwards_through_progress_option_and_tool_update() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "progress");
        let entry = with(fake.entry(json!({ "tools": ["progress"] })), json!({ "tier": "direct" }));
        let spec_json = entry.to_string();
        let h = env.host().builtins(&["mcp"]).config(servers(json!({ "fake": entry }))).no_session().build().await;
        expose(&h);
        wait_until(15, "the progress tool", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__progress') ~= nil").await }
        })
        .await;

        let script = format!(
            r#"
            local direct_seen = {{}}
            local spec = mare.json.decode({spec_lit})
            spec.name = "direct-progress"
            local client = mare.mcp.connect(spec)
            local cres = client:call("progress", {{}}, {{
                progress = function(p)
                    direct_seen[#direct_seen + 1] = p
                end,
            }})
            client:close()
            assert(cres and cres.content and cres.content[1].text == "progress-done", "unexpected direct result")

            local updates = {{}}
            local tres = T:execute({{ id = "p1", name = "mcp__fake__progress", arguments = {{}} }}, {{
                on_update = function(u)
                    updates[#updates + 1] = u
                end,
            }})
            assert(tres and not tres.is_error, "tool execution failed: " .. tostring(tres and tres.content))
            return #direct_seen,
                direct_seen[1] and direct_seen[1].progress or -1,
                direct_seen[1] and direct_seen[1].total or -1,
                direct_seen[1] and direct_seen[1].message or "",
                #updates,
                updates[1] and updates[1].progress or -1,
                updates[1] and updates[1].total or -1,
                updates[1] and updates[1].message or "",
                updates[1] and updates[1].output or ""
            "#,
            spec_lit = serde_json::to_string(&spec_json).unwrap()
        );
        let (d_count, d_prog, d_total, d_msg, u_count, u_prog, u_total, u_msg, u_out): (
            usize,
            f64,
            f64,
            String,
            usize,
            f64,
            f64,
            String,
            String,
        ) = eval(&h, &script).await;
        assert!(d_count >= 1, "client:call({{ progress }}) must receive notifications/progress");
        assert_eq!(d_prog, 1.0);
        assert_eq!(d_total, 2.0);
        assert_eq!(d_msg, "step 1");
        assert!(u_count >= 1, "tool update callback must receive progress via t_ctx.update");
        assert_eq!(u_prog, 1.0);
        assert_eq!(u_total, 2.0);
        assert_eq!(u_msg, "step 1");
        assert_eq!(u_out, "step 1");
        h.shutdown().await;
    })
    .await
}

/// Item 2 (self-written): MCP prompt images must carry `type = "image"` so `agent/inputs`
/// and provider serialization include the image block in the outgoing provider request.
#[tokio::test]
async fn mcp_prompt_image_preserves_type_and_reaches_provider_request() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/openai",
            vec![Resp::sse(
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"status\":\"in_progress\"}}\n\n\
                 event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n\
                 event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m1\",\"delta\":\"saw image\"}\n\n\
                 event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"m1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"saw image\"}]}}\n\n\
                 event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
            )],
        );
        env.set_var("OPENAI_API_KEY", "test-key");
        let fake = Fake::new(&env, "imgp");
        let entry = with(fake.entry(json!({ "rich": true, "tools": ["echo"] })), json!({ "tier": "direct" }));
        let mut cfg = servers(json!({ "fake": entry }));
        merge(
            &mut cfg,
            json!({
                "model": "openai/gpt-6.1-terra",
                "plugins": { "provider-openai": { "base_url": format!("{}/openai", server.base) } }
            }),
        );
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        wait_until(15, "img_prompt command", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__echo') ~= nil").await }
        })
        .await;
        let seen_block_type: String = eval(
            &h,
            r#"
            local seen_type = ""
            local agent_err = ""
            local off_err = C.cordis:on("turn/completed", function(ev)
                if ev.error then agent_err = tostring(ev.error) end
            end)
            local off = C.cordis:on("input/submit", function(ev)
                if ev.images and ev.images[1] then
                    seen_type = tostring(ev.images[1].type)
                end
                ev.handled = true
                local ag = C.cordis:get("agent")
                ag:wait(ag:prompt(ev.text, { images = ev.images }).turn_id)
            end)
            assert(C.cordis:get("commands"):run("/mcp__fake__img_prompt"))
            off()
            off_err()
            return seen_type .. "|" .. agent_err
            "#,
        )
        .await;
        wait_until(15, "openai provider request", || async { !server.requests().is_empty() }).await;
        assert_eq!(seen_block_type, "image|", "MCP prompt image must carry type = 'image'");
        let reqs = server.requests();
        let req_body = &reqs[0].body;
        assert!(req_body.contains("iVBORw0KGgo="), "provider request must contain the prompt image: {req_body}");
        assert!(req_body.contains("input_image"), "openai request must serialize image block as input_image: {req_body}");
        h.shutdown().await;
    })
    .await
}

/// Item 7 (self-written): high-frequency burst of 100 progress notifications over a bounded
/// channel must deliver all notifications without dropping the final ones.
#[tokio::test]
async fn mcp_progress_burst_delivers_all_notifications() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "burst");
        let entry = with(fake.entry(json!({ "rich": true, "tools": ["progress_burst"] })), json!({ "tier": "direct" }));
        let spec_json = entry.clone();
        let h = env.host().builtins(&["mcp"]).config(servers(json!({ "fake": entry }))).no_session().build().await;
        expose(&h);
        wait_until(15, "progress_burst tool", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__progress_burst') ~= nil").await }
        })
        .await;
        let script = format!(
            r#"
            local count, last_prog = 0, 0
            local spec = mare.json.decode({spec_lit})
            spec.name = "direct-burst"
            local client = mare.mcp.connect(spec)
            local res = client:call("progress_burst", {{}}, {{
                progress = function(p)
                    count = count + 1
                    last_prog = p.progress
                end,
            }})
            client:close()
            assert(res and res.content and res.content[1].text == "burst-done")
            return count, last_prog
            "#,
            spec_lit = serde_json::to_string(&spec_json.to_string()).unwrap()
        );
        let (count, last_prog): (usize, f64) = eval(&h, &script).await;
        assert_eq!(count, 100, "all 100 burst progress notifications must arrive");
        assert_eq!(last_prog, 100.0);
        h.shutdown().await;
    })
    .await
}

/// Item 8 (self-written): `list_mcp_resources` and `list_mcp_resource_templates` without `server`
/// must surface connection failures (is_error when all fail, `errors` field on partial success).
#[tokio::test]
async fn list_mcp_resources_and_templates_without_server_surface_connection_failures() {
    local(async {
        let offline =
            json!({ "command": "/nonexistent/mcp-binary-that-fails", "tier": "direct", "startup_timeout_ms": 500 });
        {
            let env = TestEnv::new().await;
            let h = env
                .host()
                .builtins(&["mcp"])
                .config(servers(json!({ "dead": offline.clone() })))
                .no_session()
                .build()
                .await;
            expose(&h);
            let (res_err, res_msg, tpl_err, tpl_msg): (bool, String, bool, String) = eval(
                &h,
                r#"
                local r = T:execute({ id = "r1", name = "list_mcp_resources", arguments = {} }, {})
                local t = T:execute({ id = "t1", name = "list_mcp_resource_templates", arguments = {} }, {})
                return r.is_error == true, tostring(r.content), t.is_error == true, tostring(t.content)
                "#,
            )
            .await;
            assert!(res_err, "all-offline list_mcp_resources must return is_error: {res_msg}");
            assert!(res_msg.contains("dead"), "{res_msg}");
            assert!(tpl_err, "all-offline list_mcp_resource_templates must return is_error: {tpl_msg}");
            assert!(tpl_msg.contains("dead"), "{tpl_msg}");
            h.shutdown().await;
        }

        // Partial success: 1 live + 1 offline server -> returns live resources + errors array.
        let env2 = TestEnv::new().await;
        let fake = Fake::new(&env2, "partial");
        let live = with(fake.entry(json!({ "rich": true, "tools": ["echo"] })), json!({ "tier": "direct" }));
        let h2 = env2
            .host()
            .builtins(&["mcp"])
            .config(servers(json!({ "live": live, "dead": offline })))
            .no_session()
            .build()
            .await;
        expose(&h2);
        wait_until(15, "live mcp tool", || {
            let h2 = h2.clone();
            async move { eval::<bool>(&h2, "return T:get('mcp__live__echo') ~= nil").await }
        })
        .await;
        let (r_json, t_json): (String, String) = eval(
            &h2,
            r#"
            local r = T:execute({ id = "r2", name = "list_mcp_resources", arguments = {} }, {})
            local t = T:execute({ id = "t2", name = "list_mcp_resource_templates", arguments = {} }, {})
            assert(not r.is_error, tostring(r.content))
            assert(not t.is_error, tostring(t.content))
            return tostring(r.content), tostring(t.content)
            "#,
        )
        .await;
        let r_val: serde_json::Value = serde_json::from_str(&r_json).unwrap();
        let t_val: serde_json::Value = serde_json::from_str(&t_json).unwrap();
        assert_eq!(r_val["errors"][0]["server"], "dead", "{r_val}");
        assert_eq!(t_val["errors"][0]["server"], "dead", "{t_val}");
        h2.shutdown().await;
    })
    .await
}

/// When the active model does not accept images, MCP image
/// tool results are replaced with an explanatory text block; when the model accepts images, the
/// image block is preserved.
#[tokio::test]
async fn mcp_image_result_is_replaced_with_text_when_model_lacks_image_support() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "img-model");
        let entry = with(fake.entry(json!({ "tools": ["image"] })), json!({ "tier": "direct" }));
        let cfg = json!({
            "models": {
                "faux/text-only": { "context_window": 100000, "max_output": 4096, "input": ["text"] },
                "faux/vision": { "context_window": 100000, "max_output": 4096, "input": ["text", "image"] }
            },
            "plugins": {
                "mcp": { "servers": { "fake": entry } }
            }
        });
        let h = env.host().builtins(&["mcp"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        wait_until(15, "image tool ready", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__image') ~= nil").await }
        })
        .await;

        let (no_img_type, no_img_text, img_type, img_mime): (String, String, String, String) = eval(
            &h,
            r#"local llm = C.cordis:get("llm")
               llm:set_model("faux/text-only", { save = false })
               local r1 = T:execute({ id = "i1", name = "mcp__fake__image", arguments = {} }, {})
               llm:set_model("faux/vision", { save = false })
               local r2 = T:execute({ id = "i2", name = "mcp__fake__image", arguments = {} }, {})
               return tostring(r1.content[1].type), tostring(r1.content[1].text or ""),
                      tostring(r2.content[1].type), tostring(r2.content[1].mime or "")"#,
        )
        .await;
        assert_eq!(no_img_type, "text", "text-only model must not receive image block: {no_img_text}");
        assert!(
            no_img_text.contains("[image (image/png) omitted: the current model doesn't accept images]"),
            "{no_img_text}"
        );
        assert_eq!((img_type.as_str(), img_mime.as_str()), ("image", "image/png"));
        h.shutdown().await;
    })
    .await
}
