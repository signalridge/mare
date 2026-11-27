//! OpenCode Go: the session header and freeform tools declared as function tools.

use super::*;

/// opencode Go: every request of a session carries `x-opencode-session` with the
/// session id — the main request and auxiliary ones that don't pass a session id themselves.
#[tokio::test]
async fn opencode_go_sends_the_session_header_on_main_and_auxiliary_requests() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENCODE_API_KEY", "test-opencode");
        let server = MockServer::start().await;
        server.route(
            "/og",
            vec![Resp::sse(fixture("openai-responses.sse")), Resp::sse(fixture("openai-responses.sse"))],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-opencode-go": { "base_url": server.base, "routes": { "responses": { "path": "/og" } } } } }),
        )
        .await;
        expose(&h);
        let sid: String = eval(
            &h,
            r#"local m = assert(LLM:resolve("opencode-go/crouton-alpha-free"))
               local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
               local sid = S:info().session_id
               -- main request (the agent passes the session id) and an auxiliary one that doesn't
               LLM:stream({ model = m, system = "S", session_id = sid, messages = msgs }, function() end)
               LLM:complete({ model = m, system = "summarize", messages = msgs })
               return sid"#,
        )
        .await;
        assert!(!sid.is_empty());
        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        for r in &reqs {
            assert!(r.path.ends_with("/og/responses"), "{}", r.path);
            assert_eq!(r.header("x-opencode-session"), Some(sid.as_str()), "same session value on every request");
            assert_eq!(r.header("authorization"), Some("Bearer test-opencode"));
            assert_eq!(r.header("session-id"), None, "Codex-only headers stay off other endpoints");
        }
        h.shutdown().await;
    })
    .await
}

/// opencode Go rejects `custom` (freeform) tools with a 400 (found by the Harbor evals): the
/// grammar tool apply_patch is declared as a function tool with an `input` parameter there.
#[tokio::test]
async fn opencode_go_declares_freeform_tools_as_function_tools() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENCODE_API_KEY", "test-opencode");
        let server = MockServer::start().await;
        server.route("/og", vec![Resp::sse(fixture("openai-responses.sse"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-opencode-go": { "base_url": server.base, "routes": { "responses": { "path": "/og" } } } } }),
        )
        .await;
        expose(&h);
        exec(
            &h,
            r#"local m = assert(LLM:resolve("opencode-go/crouton-alpha-free"))
               local def = assert(T:get("apply_patch"), "apply_patch is registered")
               local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
               LLM:stream({ model = m, system = "S", messages = msgs,
                 tools = { { name = "apply_patch", description = def.description, parameters = def.parameters } } }, function() end)"#,
        )
        .await;
        let reqs = server.requests();
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
        let tool = body["tools"].as_array().unwrap().iter().find(|t| t["name"] == "apply_patch").expect("apply_patch sent");
        assert_eq!(tool["type"], "function", "{tool}");
        assert!(tool["parameters"]["properties"]["input"].is_object(), "{tool}");
        h.shutdown().await;
    })
    .await
}
