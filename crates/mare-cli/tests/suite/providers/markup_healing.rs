//! Fixture & replay tests for provider-side leaked tool-call markup healing
//! (DeepSeek DSML / V3 template tokens, Kimi section tokens, Qwen XML / Qwen3 JSON).

use super::*;

#[tokio::test]
async fn deepseek_dsml_and_template_fixtures_heal_leaked_tool_calls_and_promote_stop_reason() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let dsml_sse = provider_fixture("compat/deepseek-dsml-leak.sse");
        let v3_sse = provider_fixture("compat/deepseek-v3-template-leak.sse");
        let quoted_sse = concat!(
            "data: {\"id\":\"chatcmpl-quote\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Use `<｜DSML｜tool_calls>` or `<｜tool_call_begin｜>` in docs.\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl-quote\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n"
        );
        let length_sse = dsml_sse.replace("\"finish_reason\":\"stop\"", "\"finish_reason\":\"length\"");

        server.route(
            "/compat",
            vec![
                Resp::sse(&dsml_sse),
                Resp::sse(&v3_sse),
                Resp::sse(quoted_sse),
                Resp::sse(&length_sse),
                // Non-DeepSeek generic model must NOT heal DSML markup in prose.
                Resp::sse(&dsml_sse),
            ],
        );

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "deepseek": { "base_url": format!("{}/compat", server.base) },
                    "plain": {
                        "wire": "openai-chat",
                        "base_url": format!("{}/compat", server.base),
                        "no_key": true,
                        "models": [{ "model_id": "generic-chat" }]
                    }
                }
            }),
        )
        .await;

        // 1. DeepSeek DSML XML split across chunks -> structured tool_call + stop_reason = "tool_use"
        let dsml = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(dsml["stop_reason"], "tool_use", "{dsml}");
        let dsml_content = dsml["content"].as_array().unwrap();
        let dsml_text = dsml_content.iter().find(|b| b["type"] == "text").expect("text block");
        assert_eq!(dsml_text["text"], "Let me read that file.\n", "{dsml}");
        let dsml_calls: Vec<&Value> = dsml_content.iter().filter(|b| b["type"] == "tool_call").collect();
        assert_eq!(dsml_calls.len(), 1, "{dsml}");
        assert_eq!(dsml_calls[0]["name"], "read");
        assert_eq!(dsml_calls[0]["arguments"], json!({ "path": "README.md", "limit": 42 }));
        assert!(dsml_calls[0]["id"].as_str().is_some_and(|id| !id.is_empty() && id.len() <= 40));
        let dsml_events: Vec<&str> =
            dsml["events"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert!(dsml_events.contains(&"tool_call_start"), "{dsml_events:?}");
        assert!(dsml_events.contains(&"tool_call_delta"), "{dsml_events:?}");

        // 2. DeepSeek V3 template tokens (including re-spelled `_` variant) -> structured tool_call
        let v3 = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(v3["stop_reason"], "tool_use", "{v3}");
        let v3_content = v3["content"].as_array().unwrap();
        let v3_text = v3_content.iter().find(|b| b["type"] == "text").expect("text block");
        assert_eq!(v3_text["text"], "Checking main. ", "{v3}");
        let v3_calls: Vec<&Value> = v3_content.iter().filter(|b| b["type"] == "tool_call").collect();
        assert_eq!(v3_calls.len(), 1, "{v3}");
        assert_eq!(v3_calls[0]["name"], "read");
        assert_eq!(v3_calls[0]["arguments"], json!({ "path": "src/main.rs" }));

        // 3. Quoted inline code spans with DSML / re-spelled template tokens stay prose
        let quoted = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(quoted["stop_reason"], "stop", "{quoted}");
        let quoted_content = quoted["content"].as_array().unwrap();
        assert!(quoted_content.iter().all(|b| b["type"] != "tool_call"), "{quoted}");
        let qtext = quoted_content.iter().find(|b| b["type"] == "text").expect("text block");
        assert!(qtext["text"].as_str().unwrap().contains("<｜DSML｜tool_calls>"), "{quoted}");
        assert!(qtext["text"].as_str().unwrap().contains("<｜tool_call_begin｜>"), "{quoted}");

        // 4. Truncated `finish_reason: "length"` never promotes to `"tool_use"`
        let truncated = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(truncated["stop_reason"], "length", "{truncated}");

        // 5. Generic non-DeepSeek model does NOT trigger DSML healing
        let plain = stream(&h, "plain/generic-chat").await;
        assert_eq!(plain["stop_reason"], "stop", "{plain}");
        let plain_content = plain["content"].as_array().unwrap();
        assert!(plain_content.iter().all(|b| b["type"] != "tool_call"), "{plain}");
        let ptext = plain_content.iter().find(|b| b["type"] == "text").expect("text block");
        assert!(ptext["text"].as_str().unwrap().contains("<｜DSML｜tool_calls>"), "{plain}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn kimi_and_qwen_fixtures_heal_leaked_tool_calls_and_preserve_surrounding_text() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MOONSHOT_API_KEY", "test-kimi");
        let server = MockServer::start().await;
        let kimi_sse = provider_fixture("compat/kimi-leak.sse");
        let qwen_xml_sse = provider_fixture("compat/qwen-xml-leak.sse");
        let qwen3_json_sse = concat!(
            "data: {\"id\":\"chatcmpl-q3\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Looking up.<tool_ca\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl-q3\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ll>\\n{\\\"name\\\":\\\"read\\\",\\\"arguments\\\":{\\\"path\\\":\\\"a.rs\\\"}}\\n</tool_call>\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl-q3\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":10}}\n\n",
            "data: [DONE]\n\n"
        );

        server.route("/kimi", vec![Resp::sse(&kimi_sse)]);
        server.route("/qwen", vec![Resp::sse(&qwen_xml_sse), Resp::sse(qwen3_json_sse)]);
        server.route("/plain", vec![Resp::sse(&kimi_sse), Resp::sse(&qwen_xml_sse)]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "kimi": { "base_url": format!("{}/kimi", server.base) },
                    "qwen": {
                        "wire": "openai-chat",
                        "dialect": "qwen",
                        "base_url": format!("{}/qwen", server.base),
                        "no_key": true,
                        "models": [{ "model_id": "qwen3-coder-plus" }]
                    },
                    "plain": {
                        "wire": "openai-chat",
                        "base_url": format!("{}/plain", server.base),
                        "no_key": true,
                        "models": [{ "model_id": "generic-chat" }]
                    }
                }
            }),
        )
        .await;

        // 1. Kimi leaked tool-call section -> normalized `read` tool call
        let kimi = stream(&h, "kimi/kimi-k2-0905-preview").await;
        assert_eq!(kimi["stop_reason"], "tool_use", "{kimi}");
        let kimi_content = kimi["content"].as_array().unwrap();
        let kimi_text = kimi_content.iter().find(|b| b["type"] == "text").expect("text block");
        assert_eq!(kimi_text["text"], "Inspecting Cargo.toml.", "{kimi}");
        let kimi_calls: Vec<&Value> = kimi_content.iter().filter(|b| b["type"] == "tool_call").collect();
        assert_eq!(kimi_calls.len(), 1, "{kimi}");
        assert_eq!(kimi_calls[0]["name"], "read");
        assert_eq!(kimi_calls[0]["arguments"], json!({ "path": "Cargo.toml" }));

        // 2. Qwen XML `<tool_calls><read .../><grep .../></tool_calls>` -> 2 tool calls
        let qwen_xml = stream(&h, "qwen/qwen3-coder-plus").await;
        assert_eq!(qwen_xml["stop_reason"], "tool_use", "{qwen_xml}");
        let qx_content = qwen_xml["content"].as_array().unwrap();
        let qx_text = qx_content.iter().find(|b| b["type"] == "text").expect("text block");
        assert_eq!(qx_text["text"], "Scanning files.", "{qwen_xml}");
        let qx_calls: Vec<&Value> = qx_content.iter().filter(|b| b["type"] == "tool_call").collect();
        assert_eq!(qx_calls.len(), 2, "{qwen_xml}");
        assert_eq!(qx_calls[0]["name"], "read");
        assert_eq!(qx_calls[0]["arguments"], json!({ "path": "lib.rs" }));
        assert_eq!(qx_calls[1]["name"], "grep");
        assert_eq!(qx_calls[1]["arguments"], json!({ "pattern": "fn main" }));

        // 3. Qwen3 `<tool_call>{...}</tool_call>` -> 1 tool call
        let qwen3 = stream(&h, "qwen/qwen3-coder-plus").await;
        assert_eq!(qwen3["stop_reason"], "tool_use", "{qwen3}");
        let q3_content = qwen3["content"].as_array().unwrap();
        let q3_text = q3_content.iter().find(|b| b["type"] == "text").expect("text block");
        assert_eq!(q3_text["text"], "Looking up.", "{qwen3}");
        let q3_calls: Vec<&Value> = q3_content.iter().filter(|b| b["type"] == "tool_call").collect();
        assert_eq!(q3_calls.len(), 1, "{qwen3}");
        assert_eq!(q3_calls[0]["name"], "read");
        assert_eq!(q3_calls[0]["arguments"], json!({ "path": "a.rs" }));

        // 4. Generic model leaves both Kimi and Qwen markup untouched
        let p_kimi = stream(&h, "plain/generic-chat").await;
        assert_eq!(p_kimi["stop_reason"], "stop", "{p_kimi}");
        assert!(p_kimi["content"].as_array().unwrap().iter().all(|b| b["type"] != "tool_call"));
        let p_qwen = stream(&h, "plain/generic-chat").await;
        assert_eq!(p_qwen["stop_reason"], "stop", "{p_qwen}");
        assert!(p_qwen["content"].as_array().unwrap().iter().all(|b| b["type"] != "tool_call"));

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn deepseek_strips_dsml_tails_from_structured_args_and_replayed_history_on_chat_and_responses() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OR_RESP_KEY", "test-or");
        let server = MockServer::start().await;

        // Response 1 (Chat): structured tool_calls carrying leaked DSML tail in arg + intentionally quoted DSML arg,
        // plus leaked DSML envelope in content (must NOT duplicate the structured tool call).
        let chat_mixed_sse = concat!(
            "data: {\"id\":\"chatcmpl-mix\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Running.<｜DSML｜tool_calls><｜DSML｜invoke name=\\\"bash\\\"><｜DSML｜parameter name=\\\"command\\\" string=\\\"true\\\">ls</｜DSML｜parameter></｜DSML｜invoke></｜DSML｜tool_calls>\",\"tool_calls\":[{\"index\":0,\"id\":\"call_s1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"ls -la</｜DSML｜ parameter><｜DSML｜parameter name=\\\"}\"}},{\"index\":1,\"id\":\"call_s2\",\"type\":\"function\",\"function\":{\"name\":\"grep\",\"arguments\":\"{\\\"pattern\\\":\\\"grep '</｜DSML｜parameter>'\\\"}\"}}]}}]}\n\n",
            "data: {\"id\":\"chatcmpl-mix\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":10}}\n\n",
            "data: [DONE]\n\n"
        );
        let chat_ok_sse = concat!(
            "data: {\"id\":\"chatcmpl-ok\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"done\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        );
        let resp_ok_sse = responses_text("done-resp");

        server.route("/compat", vec![Resp::sse(chat_mixed_sse), Resp::sse(chat_ok_sse)]);
        server.route("/or-resp", vec![Resp::sse(&resp_ok_sse)]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "deepseek": { "base_url": format!("{}/compat", server.base) },
                    "or-resp": {
                        "wire_api": "responses",
                        "dialect": "openrouter",
                        "base_url": format!("{}/or-resp", server.base),
                        "api_key_env": "OR_RESP_KEY",
                        "models": [{ "model_id": "deepseek/deepseek-v3.2" }]
                    }
                }
            }),
        )
        .await;

        // 1. Structured tool_calls with leaked DSML tail: tail cut, quoted literal preserved, no duplicate call from content
        let mixed = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(mixed["stop_reason"], "tool_use", "{mixed}");
        let m_content = mixed["content"].as_array().unwrap();
        let m_text = m_content.iter().find(|b| b["type"] == "text").expect("text block");
        assert_eq!(m_text["text"], "Running.", "{mixed}");
        let m_calls: Vec<&Value> = m_content.iter().filter(|b| b["type"] == "tool_call").collect();
        assert_eq!(m_calls.len(), 2, "must not duplicate structured tool call from leaked markup: {mixed}");
        assert_eq!(m_calls[0]["arguments"]["command"], "ls -la", "{mixed}");
        assert_eq!(m_calls[1]["arguments"]["pattern"], "grep '</｜DSML｜parameter>'", "{mixed}");

        // 2. Replay dirty assistant turn (broken DSML in text + reasoning + args, plus quoted code span) on Chat & Responses
        exec(
            &h,
            r#"
            local history = {
                { role = "user", content = { { type = "text", text = "check" } } },
                {
                    role = "assistant",
                    provider_id = "deepseek",
                    model_id = "deepseek-chat",
                    content = {
                        { type = "reasoning", text = "Think first.\n<｜DSML｜invoke name=\"bash\">\n<｜DSML｜parameter name=\"cmd\">ls</｜DSML｜parameter>\n</｜DSML｜invoke>" },
                        { type = "text", text = "Before.\nbash\n<｜DSML｜parameter name=\"cmd\">ls</｜DSML｜parameter>\nKeep `</｜DSML｜parameter>` quoted." },
                        { type = "tool_call", id = "call_r1", name = "bash", arguments = { command = "pwd</｜DSML｜parameter>" } },
                    },
                },
                { role = "tool_result", tool_call_id = "call_r1", tool_name = "bash", content = { { type = "text", text = "/tmp" } } },
                { role = "user", content = { { type = "text", text = "next" } } },
            }
            local ds = assert(LLM:resolve("deepseek/deepseek-chat"))
            LLM:stream({ model = ds, system = "S", messages = history }, function() end)

            local resp_history = {
                { role = "user", content = { { type = "text", text = "check" } } },
                {
                    role = "assistant",
                    provider_id = "or-resp",
                    model_id = "deepseek/deepseek-v3.2",
                    content = {
                        {
                            type = "reasoning",
                            text = "Think.\n<｜DSML｜invoke name=\"bash\"></｜DSML｜invoke>",
                            native = {
                                ["openai-responses"] = {
                                    item = {
                                        type = "reasoning",
                                        summary = { { type = "summary_text", text = "Plan.\n<｜DSML｜invoke name=\"bash\"></｜DSML｜invoke>" } },
                                    },
                                },
                            },
                        },
                        { type = "text", text = "Hello.\n<｜DSML｜invoke name=\"read\">\n<｜DSML｜parameter name=\"path\">a</｜DSML｜parameter>\n</｜DSML｜invoke>\nUse `<｜DSML｜invoke>` in docs." },
                        { type = "tool_call", id = "call_rr1", name = "bash", arguments = { command = "whoami</｜DSML｜parameter>" } },
                    },
                },
                { role = "tool_result", tool_call_id = "call_rr1", tool_name = "bash", content = { { type = "text", text = "riku" } } },
                { role = "user", content = { { type = "text", text = "next" } } },
            }
            local or_m = assert(LLM:resolve("or-resp/deepseek/deepseek-v3.2"))
            LLM:stream({ model = or_m, system = "S", messages = resp_history }, function() end)
            "#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 3, "1 mixed + 1 chat replay + 1 responses replay");

        // Verify Chat Completions replay cleaned unquoted DSML while preserving quoted code span
        let chat_body = reqs[1].json();
        let chat_asst = chat_body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message");
        let chat_text = chat_asst["content"].as_str().unwrap();
        assert!(chat_text.contains("Before."), "{chat_asst}");
        assert!(chat_text.contains("`</｜DSML｜parameter>`"), "quoted code span must survive: {chat_asst}");
        assert!(!chat_text.contains("<｜DSML｜parameter name=\"cmd\">"), "unquoted leak must be stripped: {chat_asst}");
        let chat_rc = chat_asst["reasoning_content"].as_str().unwrap();
        assert!(!chat_rc.contains("<｜DSML｜invoke"), "reasoning leak must be stripped: {chat_asst}");
        let chat_args = chat_asst["tool_calls"][0]["function"]["arguments"].as_str().unwrap();
        assert!(!chat_args.contains("DSML"), "replayed tool call arg must be cleaned: {chat_asst}");
        assert!(chat_args.contains("pwd"), "{chat_asst}");

        // Verify Responses API replay cleaned native reasoning item, output_text, and function_call arguments
        let resp_body = reqs[2].json();
        let resp_input = resp_body["input"].as_array().unwrap();
        let r_reason = resp_input.iter().find(|i| i["type"] == "reasoning").expect("reasoning item");
        assert!(
            !r_reason.to_string().contains("<｜DSML｜invoke"),
            "Responses native reasoning item must be stripped: {r_reason}"
        );
        let r_msg = resp_input.iter().find(|i| i["type"] == "message").expect("message item");
        let r_text = r_msg["content"][0]["text"].as_str().unwrap();
        assert!(r_text.contains("Hello."), "{r_msg}");
        assert!(r_text.contains("`<｜DSML｜invoke>`"), "quoted code span must survive: {r_msg}");
        assert!(!r_text.contains("name=\"read\""), "unquoted DSML must be stripped: {r_msg}");
        let r_fc = resp_input.iter().find(|i| i["type"] == "function_call").expect("function_call item");
        let r_fc_args = r_fc["arguments"].as_str().unwrap();
        assert!(!r_fc_args.contains("DSML"), "Responses function_call args must be cleaned: {r_fc}");
        assert!(r_fc_args.contains("whoami"), "{r_fc}");

        h.shutdown().await;
    })
    .await
}
