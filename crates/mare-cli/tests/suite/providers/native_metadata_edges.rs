//! Edge-case tests for wire-native metadata, freeform custom calls across model switches,
//! Anthropic hosted tool result attachment & replay, cross-wire hosted call stripping,
//! disk reload persistence, agent loop hosted-call isolation, compaction replay,
//! unknown model-field validation, and Anthropic cache breakpoint capping.

use super::*;

async fn full_provider_host(env: &mut TestEnv, server: &MockServer, extra: Value) -> Arc<Host> {
    for (k, v) in [
        ("OPENAI_API_KEY", "test-openai"),
        ("ANTHROPIC_API_KEY", "test-anthropic"),
        ("DEEPSEEK_API_KEY", "test-deepseek"),
        ("GEMINI_API_KEY", "test-google"),
    ] {
        env.set_var(k, v);
    }
    let base = &server.base;
    let h = env
        .host()
        .builtins(&["tools-apply-patch", "mcp", "usage", "compact", "user-config", "jobs"])
        .config(json!({
            "providers": { "deepseek": { "base_url": format!("{base}/compat") } },
            "plugins": {
                "provider-openai": { "base_url": format!("{base}/openai") },
                "provider-anthropic": { "base_url": format!("{base}/anthropic") },
                "provider-google": { "base_url": format!("{base}/google") },
                "llm": { "retry_base_ms": 1 },
            },
        }))
        .config(extra)
        .build_exposed()
        .await;
    global(&h, "LLM", h.service_table("llm").unwrap());
    global(&h, "COMP", h.service_table("compact").unwrap());
    h
}

#[tokio::test]
async fn openai_freeform_custom_tool_call_survives_same_wire_model_switch() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        let sse = [
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_patch_1","name":"apply_patch","input":"*** Begin Patch\n*** End Patch"}}"#,
            r#"data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            "",
        ]
        .join("\n\n");
        server.route("/openai", vec![Resp::sse(sse), Resp::sse(responses_text("switched ok"))]);

        let h = provider_host(&mut env, &server, json!({})).await;
        exec(
            &h,
            r#"
            local terra = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local sol = assert(LLM:resolve("openai/gpt-6.1-sol"))
            local tools = { { name = "apply_patch", description = "patch", parameters = { type = "object", properties = { input = { type = "string" } } } } }
            local m1 = LLM:stream({
              model = terra,
              system = "S",
              tools = tools,
              messages = { { role = "user", content = { { type = "text", text = "patch file" } } } },
            }, function() end)
            assert(m1.content[1].type == "tool_call" and m1.content[1].name == "apply_patch", "expected apply_patch call")
            assert(m1.content[1].native == nil, "custom flag must not depend on model-scoped native bag")
            -- Switch to another model on the same OpenAI Responses wire.
            LLM:stream({
              model = sol,
              system = "S",
              tools = tools,
              messages = {
                { role = "user", content = { { type = "text", text = "patch file" } } },
                m1,
                { role = "tool_result", tool_call_id = "call_patch_1", tool_name = "apply_patch", content = { { type = "text", text = "patched" } } },
              },
            }, function() end)
            "#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        let input = reqs[1].json()["input"].as_array().unwrap().clone();
        let call_item = input
            .iter()
            .find(|it| it["call_id"] == "call_patch_1" && it["name"] == "apply_patch")
            .expect("missing replayed apply_patch call");
        assert_eq!(
            call_item["type"], "custom_tool_call",
            "freeform apply_patch must stay custom_tool_call across model switch: {input:?}"
        );
        assert_eq!(call_item["input"], "*** Begin Patch\n*** End Patch");
        let out_item = input
            .iter()
            .find(|it| it["call_id"] == "call_patch_1" && it["output"].is_string())
            .expect("missing replayed apply_patch output");
        assert_eq!(
            out_item["type"], "custom_tool_call_output",
            "freeform output must stay custom_tool_call_output: {input:?}"
        );

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn anthropic_hosted_tool_attaches_result_delays_done_and_replays_same_wire_only() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        let ev = |t: &str, d: Value| format!("event: {t}\ndata: {d}\n\n");
        let ant_hosted_sse = [
            ev("message_start", json!({"type":"message_start","message":{"usage":{"input_tokens":15}}})),
            ev("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}})),
            ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"mare\"}"}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":0})),
            ev("content_block_start", json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","url":"https://example.com","title":"Mare"}]}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":1})),
            ev("content_block_start", json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}})),
            ev("content_block_delta", json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Found Mare."}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":2})),
            ev("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}})),
            ev("message_stop", json!({"type":"message_stop"})),
        ]
        .concat();
        let ant_ok_sse = [
            ev("message_start", json!({"type":"message_start","message":{"usage":{"input_tokens":10}}})),
            ev("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":0})),
            ev("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}})),
            ev("message_stop", json!({"type":"message_stop"})),
        ]
        .concat();
        let google_ok_sse = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"gem ok\"}]},\"finishReason\":\"STOP\"}]}\n\n";
        server.route("/anthropic", vec![Resp::sse(ant_hosted_sse), Resp::sse(ant_ok_sse)]);
        server.route("/openai", vec![Resp::sse(responses_text("oai ok"))]);
        server.route("/google", vec![Resp::sse(google_ok_sse.to_string())]);

        let h = provider_host(&mut env, &server, json!({})).await;
        let out_json: String = eval(
            &h,
            r#"
            local ant = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local oai = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local gem = assert(LLM:resolve("google/gemini-3-pro-preview"))
            local events = {}
            local m1 = LLM:stream({
              model = ant,
              system = "S",
              messages = { { role = "user", content = { { type = "text", text = "search mare" } } } },
            }, function(e) events[#events + 1] = e end)
            local history = {
              { role = "user", content = { { type = "text", text = "search mare" } } },
              m1,
              { role = "user", content = { { type = "text", text = "follow up" } } },
            }
            -- Same-wire replay
            LLM:stream({ model = ant, system = "S", messages = history }, function() end)
            -- Cross-wire replays (OpenAI + Google) + generic transform_messages check
            LLM:stream({ model = oai, system = "S", messages = history }, function() end)
            LLM:stream({ model = gem, system = "S", messages = history }, function() end)
            local transformed = LLM:transform_messages(history, oai, {})
            return mare.json.encode({ msg = m1, events = events, transformed = transformed })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();

        // Stream events: start -> done (with result attached) -> text_delta
        let evs = out["events"].as_array().unwrap();
        let types: Vec<&str> = evs.iter().filter_map(|e| e["type"].as_str()).collect();
        assert_eq!(types, vec!["hosted_tool_start", "hosted_tool_done", "text_delta"], "{evs:?}");
        assert_eq!(evs[1]["id"], "srvtoolu_1");
        assert_eq!(evs[1]["status"], "completed");
        assert_eq!(evs[1]["result"]["status"], "completed");
        assert_eq!(evs[1]["result"]["content"][0]["url"], "https://example.com");

        // Persisted message has 2 blocks: hosted tool_call (with result + both native blocks) and text;
        // NO separate anthropic-messages.web_search_tool_result block.
        let content = out["msg"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2, "result must be attached to hosted tool_call, not separate block: {content:?}");
        assert_eq!(content[0]["type"], "tool_call");
        assert_eq!(content[0]["hosted"], true);
        assert_eq!(content[0]["id"], "srvtoolu_1");
        assert_eq!(content[0]["arguments"]["query"], "mare");
        assert_eq!(content[0]["result"]["status"], "completed");
        assert_eq!(content[0]["native"]["anthropic-messages"]["block"]["type"], "server_tool_use");
        assert_eq!(content[0]["native"]["anthropic-messages"]["result_block"]["type"], "web_search_tool_result");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "Found Mare.");

        // Generic cross-model transform_messages drops the hosted call so third-party wires never see it.
        let ta_content = out["transformed"][1]["content"].as_array().unwrap();
        assert_eq!(ta_content.len(), 1, "cross-model transform must drop hosted call: {ta_content:?}");
        assert_eq!(ta_content[0]["type"], "text");

        // Verify wire request bodies:
        // reqs[1] (Anthropic same-wire): emits server_tool_use, then web_search_tool_result, then text
        let reqs = server.requests();
        assert_eq!(reqs.len(), 4);
        let ant_blocks = reqs[1].json()["messages"][1]["content"].as_array().unwrap().clone();
        assert_eq!(ant_blocks.len(), 3, "{ant_blocks:?}");
        assert_eq!(ant_blocks[0]["type"], "server_tool_use");
        assert_eq!(ant_blocks[0]["id"], "srvtoolu_1");
        assert_eq!(ant_blocks[1]["type"], "web_search_tool_result");
        assert_eq!(ant_blocks[1]["tool_use_id"], "srvtoolu_1");
        assert_eq!(ant_blocks[2]["type"], "text");

        // reqs[2] (OpenAI cross-wire) and reqs[3] (Google cross-wire): no tool call leaked
        let oai_input = reqs[2].json()["input"].as_array().unwrap().clone();
        assert!(
            oai_input.iter().all(|it| it["type"] != "function_call" && it["type"] != "web_search_call"),
            "cross-wire OpenAI must not receive foreign hosted call: {oai_input:?}"
        );
        let gem_contents = reqs[3].json()["contents"].as_array().unwrap().clone();
        let gem_parts = gem_contents[1]["parts"].as_array().unwrap();
        assert!(
            gem_parts.iter().all(|p| p.get("functionCall").is_none()),
            "cross-wire Google must not receive foreign hosted call: {gem_parts:?}"
        );

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn session_reload_from_disk_preserves_native_empty_summary_redacted_data_and_hosted_calls() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(responses_text("after reload"))]);

        let h1 = full_provider_host(&mut env, &server, json!({})).await;
        let session_path: String = eval(
            &h1,
            r#"
            S:persist_message({
              message = {
                role = "user",
                content = { { type = "text", text = "hi" } },
              },
            })
            S:persist_message({
              message = {
                role = "assistant",
                provider_id = "openai",
                model_id = "gpt-6.1-terra",
                stop_reason = "stop",
                content = {
                  {
                    type = "reasoning",
                    text = "",
                    native = {
                      ["openai-responses"] = {
                        item = { id = "rs_empty", type = "reasoning", summary = mare.json.array(), encrypted_content = "enc-empty" },
                      },
                    },
                  },
                  {
                    type = "reasoning",
                    text = "",
                    redacted = true,
                    native = {
                      ["anthropic-messages"] = { redacted_data = "ant-redacted-blob" },
                    },
                  },
                  {
                    type = "tool_call",
                    id = "ws_reload_1",
                    name = "web_search",
                    arguments = { type = "search", query = "rust" },
                    hosted = true,
                    result = { status = "completed" },
                    native = {
                      ["openai-responses"] = {
                        item = { id = "ws_reload_1", type = "web_search_call", status = "completed", action = { type = "search", query = "rust" } },
                      },
                    },
                  },
                  { type = "text", text = "saved answer" },
                },
              },
            })
            return assert(S:info().path)
            "#,
        )
        .await;
        h1.shutdown().await;

        // Boot a brand new host and reload the session from disk.
        let h2 = full_provider_host(&mut env, &server, json!({})).await;
        let out_json: String = eval(
            &h2,
            &format!(
                r#"
                assert(S:load({{ path = {:?} }}))
                local msgs = A:messages()
                local oa = assert(LLM:resolve("openai/gpt-6.1-terra"))
                local replay_msgs = {{}}
                for i, m in ipairs(msgs) do replay_msgs[i] = m end
                replay_msgs[#replay_msgs + 1] = {{ role = "user", content = {{ {{ type = "text", text = "continue" }} }} }}
                LLM:stream({{ model = oa, system = "S", messages = replay_msgs }}, function() end)
                return mare.json.encode(msgs)
                "#,
                session_path
            ),
        )
        .await;
        let msgs: Value = serde_json::from_str(&out_json).unwrap();
        let blocks = msgs[1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["native"]["openai-responses"]["item"]["summary"], json!([]), "empty summary array must survive JSONL reload: {blocks:?}");
        assert_eq!(blocks[1]["native"]["anthropic-messages"]["redacted_data"], "ant-redacted-blob");
        assert_eq!(blocks[2]["hosted"], true);
        assert_eq!(blocks[2]["result"]["status"], "completed");

        // Replaying reloaded session to OpenAI sends reasoning item (with summary: []) and web_search_call intact.
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        let input = reqs[0].json()["input"].as_array().unwrap().clone();
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["summary"], json!([]));
        assert_eq!(input[1]["encrypted_content"], "enc-empty");
        assert_eq!(input[2]["type"], "web_search_call");
        assert_eq!(input[2]["id"], "ws_reload_1");

        h2.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn agent_loop_skips_hosted_calls_for_execution_and_approval_and_compaction_replays() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        let long_report = format!("Sunny today. {}", "Detailed forecast notes. ".repeat(80));
        let hosted_turn_sse = [
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"ws_agent_1","type":"web_search_call","status":"completed","action":{"type":"search","query":"weather"}}}"#.to_string(),
            format!("data: {}", json!({"type":"response.output_text.delta","output_index":1,"item_id":"msg_a1","content_index":0,"delta": long_report})),
            format!("data: {}", json!({"type":"response.output_item.done","output_index":1,"item":{"id":"msg_a1","type":"message","role":"assistant","content":[{"type":"output_text","text": long_report,"annotations":[]}]}})),
            r#"data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":20,"output_tokens":500}}}"#.to_string(),
            String::new(),
        ]
        .join("\n\n");
        // 1) Hosted-tool turn, 2) local compaction summary, 3) post-compaction follow-up turn.
        server.route(
            "/openai",
            vec![
                Resp::sse(hosted_turn_sse),
                Resp::sse(responses_text("Summary: weather sunny.")),
                Resp::sse(responses_text("Still sunny.")),
            ],
        );

        let h = full_provider_host(
            &mut env,
            &server,
            json!({
                "model": "openai/gpt-6.1-terra",
                "plugins": { "compact": { "remote": false, "keep_recent_tokens": 0 } },
            }),
        )
        .await;
        let out_json: String = eval(
            &h,
            r#"
            local tool_before_calls = 0
            local approval_calls = 0
            C.cordis:on("tool/before", function()
              tool_before_calls = tool_before_calls + 1
              return { decision = "allow" }
            end)
            C.cordis:on("approval/requested", function()
              approval_calls = approval_calls + 1
            end)

            local t1 = A:prompt("check weather")
            A:wait(t1.turn_id)
            local msgs_after_turn = A:messages()

            -- Compact locally (turn contains hosted web_search_call + text), then run a follow-up turn.
            local comp_res = COMP:run()
            local t2 = A:prompt("any update?")
            A:wait(t2.turn_id)

            return mare.json.encode({
              tool_before_calls = tool_before_calls,
              approval_calls = approval_calls,
              msgs_after_turn = msgs_after_turn,
              compacted = comp_res ~= nil,
            })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();
        assert_eq!(out["tool_before_calls"], 0, "hosted tool_call must never enter tool/before: {out}");
        assert_eq!(out["approval_calls"], 0, "hosted tool_call must never trigger approval: {out}");
        let msgs = out["msgs_after_turn"].as_array().unwrap();
        assert!(
            msgs.iter().all(|m| m["role"] != "tool_result"),
            "hosted tool_call must never generate orphan tool_result: {msgs:?}"
        );
        assert_eq!(out["compacted"], true);

        // All 3 OpenAI requests succeeded (turn 1 + local summary + post-compaction turn 2).
        let reqs = server.requests();
        assert_eq!(reqs.len(), 3, "expected hosted turn, compaction summary, and post-compaction turn");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn wire_validation_checks_unknown_model_fields_dotted_names_and_anthropic_cache_cap() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let ev = |t: &str, d: Value| format!("event: {t}\ndata: {d}\n\n");
        let ant_ok_sse = [
            ev("message_start", json!({"type":"message_start","message":{"usage":{"input_tokens":10}}})),
            ev(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}),
            ),
            ev("content_block_stop", json!({"type":"content_block_stop","index":0})),
            ev(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            ),
            ev("message_stop", json!({"type":"message_stop"})),
        ]
        .concat();
        server.route("/anthropic", vec![Resp::sse(ant_ok_sse)]);

        let h = provider_host(&mut env, &server, json!({})).await;
        let out_json: String = eval(
            &h,
            r#"
            local validate = require("llm.validate")
            -- 1) Wire names containing `.` are forbidden so `<wire>.<name>` extension blocks stay unambiguous.
            local dot_ok, _, dot_err = validate.wire("acme.v1", {
              encode = function() return {} end,
              decoder = function() return {} end,
            })
            -- 2) Unknown model fields are reported by validate.provider against generic U wire model_fields.
            local pok, _, _, p_unknown = validate.provider({
              provider = "acme",
              stream = function() return { role = "assistant", content = {} } end,
              models = { { model_id = "m1", typo_field = true, acme_tier = "pro" } },
            }, nil, { model_fields = { acme_tier = "string" } })
            -- 3) Anthropic block-level cache_control is capped so total breakpoints (system + messages) <= 4.
            local ant = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local cc = { ["anthropic-messages"] = { cache_control = { type = "ephemeral" } } }
            LLM:stream({
              model = ant,
              system = "System prompt",
              messages = {
                {
                  role = "user",
                  content = {
                    { type = "text", text = "b1", native = cc },
                    { type = "text", text = "b2", native = cc },
                    { type = "text", text = "b3", native = cc },
                    { type = "text", text = "b4", native = cc },
                    { type = "text", text = "b5" },
                  },
                },
              },
            }, function() end)
            return mare.json.encode({
              dot_ok = dot_ok,
              dot_err = dot_err,
              pok = pok,
              p_unknown = p_unknown,
            })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();
        assert_eq!(out["dot_ok"], false, "dotted wire name must be rejected: {out}");
        assert!(out["dot_err"].as_str().is_some_and(|s| s.contains("`.`")), "{out}");
        assert_eq!(out["pok"], true);
        assert_eq!(
            out["p_unknown"],
            json!(["models[1].typo_field"]),
            "unknown model field must be flagged while declared wire field passes: {out}"
        );

        // Count total cache_control breakpoints across system + messages on Anthropic request: must be <= 4.
        let req = server.requests().pop().unwrap().json();
        let sys_cc = req["system"].as_array().unwrap().iter().filter(|b| b.get("cache_control").is_some()).count();
        let msg_cc: usize = req["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().unwrap())
            .filter(|b| b.get("cache_control").is_some())
            .count();
        assert!(
            sys_cc + msg_cc <= 4,
            "Anthropic total cache_control breakpoints exceeded 4: sys={sys_cc}, msg={msg_cc}, req={req}"
        );
        assert_eq!(sys_cc, 1);
        assert_eq!(msg_cc, 3, "expected 2 explicit block breakpoints + 1 automatic last-user breakpoint: {req}");

        h.shutdown().await;
    })
    .await
}
