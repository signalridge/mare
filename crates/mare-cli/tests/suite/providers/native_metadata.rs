//! Tests for the unified `native` wire-metadata bag, dotted extension blocks, hosted tools
//! as `tools` entries, `WireSpec.model_fields` / `provider_fields`, citation deduplication,
//! and unified `reasoning` vocabulary across wires and history transformation.

use super::*;

#[tokio::test]
async fn native_bag_roundtrips_wire_metadata_and_strips_across_models() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        let ev = |t: &str, d: Value| format!("event: {t}\ndata: {d}\n\n");
        let anthropic_sse = [
            ev("message_start", json!({"type":"message_start","message":{"usage":{"input_tokens":10}}})),
            ev("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}})),
            ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"step 1"}})),
            ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-ant-1"}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":0})),
            ev("content_block_start", json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"enc-ant-1"}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":1})),
            ev("content_block_start", json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}})),
            ev("content_block_delta", json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"done"}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":2})),
            ev("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}})),
            ev("message_stop", json!({"type":"message_stop"})),
        ]
        .concat();
        let anthropic_replay_sse = [
            ev("message_start", json!({"type":"message_start","message":{"usage":{"input_tokens":12}}})),
            ev("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":0})),
            ev("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}})),
            ev("message_stop", json!({"type":"message_stop"})),
        ]
        .concat();
        server.route("/anthropic", vec![Resp::sse(anthropic_sse), Resp::sse(anthropic_replay_sse)]);

        let h = provider_host(&mut env, &server, json!({})).await;
        let out_json: String = eval(
            &h,
            r#"
            local ant = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local oai = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local deltas = {}
            local m1 = LLM:stream({
              model = ant,
              system = "S",
              messages = {
                {
                  role = "user",
                  content = {
                    {
                      type = "text",
                      text = "hi",
                      native = { ["anthropic-messages"] = { cache_control = { type = "ephemeral", ttl = "1h" } } },
                    },
                  },
                },
              },
            }, function(e) deltas[#deltas + 1] = e.type end)

            -- Replay to same Anthropic model preserves reasoning signatures + redacted_data.
            local history = {
              { role = "user", content = { { type = "text", text = "hi" } } },
              m1,
              { role = "user", content = { { type = "text", text = "continue" } } },
            }
            LLM:stream({ model = ant, system = "S", messages = history }, function() end)

            -- Cross-model transform_messages strips ALL native bags (including third-party wire keys)
            -- and converts reasoning + extension blocks via one generic rule.
            local custom_history = {
              {
                role = "assistant",
                provider_id = "acme",
                model_id = "acme-1",
                native = { ["acme-wire"] = { msg_token = "tok" } },
                content = {
                  { type = "reasoning", text = "thought", native = { ["acme-wire"] = { sig = "s" } } },
                  { type = "reasoning", text = "", redacted = true, native = { ["acme-wire"] = { enc = "e" } } },
                  { type = "text", text = "answer", native = { ["acme-wire"] = { ann = 1 } } },
                  { type = "tool_call", id = "c1", name = "read", arguments = {}, native = { ["acme-wire"] = { ts = "t" } } },
                  { type = "acme-wire.card", text = "fallback text", native = { ["acme-wire"] = { raw = 1 } } },
                  { type = "acme-wire.opaque", native = { ["acme-wire"] = { raw = 2 } } },
                },
              },
              { role = "tool_result", tool_call_id = "c1", tool_name = "read", content = { { type = "text", text = "ok" } } },
            }
            local transformed = LLM:transform_messages(custom_history, oai, { { name = "read" } })
            return mare.json.encode({ m1 = m1, deltas = deltas, transformed = transformed })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();

        assert!(
            out["deltas"].as_array().unwrap().iter().any(|v| v == "reasoning_delta"),
            "expected reasoning_delta, got {:?}",
            out["deltas"]
        );
        assert!(
            !out["deltas"].as_array().unwrap().iter().any(|v| v == "thinking_delta"),
            "thinking_delta must not be emitted: {:?}",
            out["deltas"]
        );

        let content = out["m1"]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "reasoning");
        assert_eq!(content[0]["text"], "step 1");
        assert!(content[0].get("signature").is_none(), "no legacy signature field: {}", content[0]);
        assert_eq!(content[0]["native"]["anthropic-messages"]["signature"], "sig-ant-1");

        assert_eq!(content[1]["type"], "reasoning");
        assert_eq!(content[1]["redacted"], true);
        assert!(content[1].get("signature").is_none(), "no legacy signature field: {}", content[1]);
        assert_eq!(content[1]["native"]["anthropic-messages"]["redacted_data"], "enc-ant-1");

        // Request 1 honored input block-level native["anthropic-messages"].cache_control
        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].json()["messages"][0]["content"][0]["cache_control"]["ttl"], "1h");

        // Request 2 replayed both thinking signature and redacted_thinking cleanly
        let replay_blocks = reqs[1].json()["messages"][1]["content"].as_array().unwrap().clone();
        assert_eq!(replay_blocks[0]["type"], "thinking");
        assert_eq!(replay_blocks[0]["signature"], "sig-ant-1");
        assert_eq!(replay_blocks[1]["type"], "redacted_thinking");
        assert_eq!(replay_blocks[1]["data"], "enc-ant-1");

        // Generic cross-model transform_messages stripped ALL native bags and resolved extension blocks
        let ta = &out["transformed"][0];
        assert!(ta.get("native").is_none(), "message native stripped: {ta}");
        let tb = ta["content"].as_array().unwrap();
        assert_eq!(tb.len(), 4, "expected reasoning->text, text, tool_call, card->text (redacted+opaque dropped): {tb:?}");
        assert_eq!(tb[0], json!({ "type": "text", "text": "thought" }));
        assert_eq!(tb[1], json!({ "type": "text", "text": "answer" }));
        assert_eq!(tb[2]["type"], "tool_call");
        assert!(tb[2].get("native").is_none(), "tool_call native stripped: {}", tb[2]);
        assert_eq!(tb[3], json!({ "type": "text", "text": "fallback text" }));

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn hosted_tools_and_citations_use_unified_shape_and_events() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        let sse = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"ws_1","type":"web_search_call","status":"in_progress","action":{"type":"search","query":"mare docs"}}}"#,
            r#"data: {"type":"response.web_search_call.searching","output_index":0,"item_id":"ws_1"}"#,
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"ws_1","type":"web_search_call","status":"completed","action":{"type":"search","query":"mare docs"}}}"#,
            r#"data: {"type":"response.output_item.added","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[]}}"#,
            r#"data: {"type":"response.output_text.delta","output_index":1,"item_id":"msg_1","content_index":0,"delta":"See docs."}"#,
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"See docs.","annotations":[{"type":"url_citation","url":"https://example.com/docs","title":"Docs","start_index":4,"end_index":8}]}]}}"#,
            r#"data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            "",
        ]
        .join("\n\n");
        server.route("/openai", vec![Resp::sse(sse), Resp::sse(responses_text("replayed"))]);
        let ant_sse = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"cross\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]
        .concat();
        server.route("/anthropic", vec![Resp::sse(ant_sse)]);

        let h = provider_host(&mut env, &server, json!({})).await;
        let out_json: String = eval(
            &h,
            r#"
            local oa = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local ant = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local desc = assert(LLM:describe_wire("openai-responses"))
            local events = {}
            local m1 = LLM:stream({
              model = oa,
              system = "S",
              messages = { { role = "user", content = { { type = "text", text = "find docs" } } } },
              tools = {
                { type = "hosted", name = "web_search", options = { mode = "live", search_context_size = "high" } },
              },
            }, function(e) events[#events + 1] = e end)
            local history = {
              { role = "user", content = { { type = "text", text = "find docs" } } },
              m1,
              { role = "user", content = { { type = "text", text = "next" } } },
            }
            LLM:stream({ model = oa, system = "S", messages = history }, function() end)
            LLM:stream({ model = ant, system = "S", messages = history }, function() end)
            return mare.json.encode({ desc = desc, msg = m1, events = events })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();

        // Wire advertises hosted_tools
        assert_eq!(out["desc"]["hosted_tools"], json!(["web_search"]));

        // Request body encoded the hosted tool entry from req.tools
        let reqs = server.requests();
        assert_eq!(reqs.len(), 3);
        let req0_tools = reqs[0].json()["tools"].as_array().unwrap().clone();
        assert_eq!(req0_tools[0]["type"], "web_search");
        assert_eq!(req0_tools[0]["external_web_access"], true);
        assert_eq!(req0_tools[0]["search_context_size"], "high");

        // Stream events use generic hosted_tool_* with name
        let event_types: Vec<&str> =
            out["events"].as_array().unwrap().iter().filter_map(|e| e["type"].as_str()).collect();
        assert_eq!(
            event_types,
            vec!["hosted_tool_start", "hosted_tool_update", "hosted_tool_done", "text_delta"],
            "{event_types:?}"
        );
        assert_eq!(out["events"][0]["name"], "web_search");
        assert_eq!(out["events"][1]["name"], "web_search");
        assert_eq!(out["events"][2]["name"], "web_search");

        // Hosted tool call stored as tool_call { hosted = true, result, native } and stop_reason stays "stop"
        assert_eq!(out["msg"]["stop_reason"], "stop", "hosted tool_call must not trigger stop_reason=tool_use");
        let content = out["msg"]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_call");
        assert_eq!(content[0]["hosted"], true);
        assert_eq!(content[0]["name"], "web_search");
        assert_eq!(content[0]["id"], "ws_1");
        assert_eq!(content[0]["result"]["status"], "completed");
        assert_eq!(content[0]["native"]["openai-responses"]["item"]["type"], "web_search_call");

        // Text block has normalized citations ONLY; raw annotations moved to native["openai-responses"]
        assert_eq!(content[1]["type"], "text");
        assert!(content[1].get("annotations").is_none(), "no duplicate top-level annotations: {}", content[1]);
        assert_eq!(content[1]["citations"][0]["url"], "https://example.com/docs");
        assert_eq!(
            content[1]["native"]["openai-responses"]["annotations"][0]["url"],
            "https://example.com/docs"
        );

        // Same-wire replay sends web_search_call and raw annotations back; cross-wire drops both
        let replay_input = reqs[1].json()["input"].as_array().unwrap().clone();
        assert_eq!(replay_input[1]["type"], "web_search_call");
        assert_eq!(replay_input[2]["content"][0]["annotations"][0]["url"], "https://example.com/docs");
        let ant_msgs = reqs[2].json()["messages"].as_array().unwrap().clone();
        assert_eq!(ant_msgs[1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(ant_msgs[1]["content"][0]["text"], "See docs.");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_endpoint_populates_hosted_tools_on_both_api_key_and_oauth_paths() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = server.base.clone();
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-openai": {
                        "codex_base_url": format!("{base}/codex"),
                        "auth_base_url": format!("{base}/auth"),
                        "web_search": "live"
                    }
                }
            }),
        )
        .await;
        let out_json: String = eval(
            &h,
            r#"
            local prov = assert(LLM:provider("openai"))
            local api_ep = assert(prov.endpoint({}))

            -- Switch to OAuth credential (stored login takes precedence over OPENAI_API_KEY env).
            mare.fs.write(
              mare.paths.auth,
              mare.json.encode({
                version = 1,
                ["openai-codex"] = {
                  type = "oauth",
                  access = "tok-access",
                  refresh = "tok-refresh",
                  expires = (os.time() + 3600) * 1000,
                  account_id = "acct-1",
                },
              }),
              { mode = 384 }
            )
            local oauth_ep = assert(prov.endpoint({}))
            return mare.json.encode({
              prov_ht = prov.hosted_tools,
              api_codex = api_ep.codex,
              api_ht = api_ep.hosted_tools,
              oauth_codex = oauth_ep.codex,
              oauth_ht = oauth_ep.hosted_tools,
            })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();
        let expected = json!({ "web_search": { "supported": true, "mode": "live" } });
        assert_eq!(out["prov_ht"], expected);
        assert_eq!(out["api_codex"], false);
        assert_eq!(
            out["api_ht"], expected,
            "API-key openai_endpoint must populate hosted_tools from local OPENAI_HOSTED_TOOLS: {out}"
        );
        assert_eq!(out["oauth_codex"], true);
        assert_eq!(
            out["oauth_ht"], expected,
            "OAuth openai_endpoint must populate hosted_tools from local OPENAI_HOSTED_TOOLS: {out}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn wire_spec_model_fields_and_extension_text_of_work_uniformly() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;
        let out_json: String = eval(
            &h,
            r#"
            LLM:register_wire({
              name = "acme-wire",
              route = "acme",
              hosted_tools = { "code_exec" },
              provider_fields = { acme_flag = "boolean" },
              model_fields = { acme_tier = "string" },
              encode = function(req, ep) return { url = ep.base_url, body = {} } end,
              decoder = function() return { event = function() end, finish = function() return { role = "assistant", content = {} } end } end,
              provider = function(name, entry)
                return {
                  route = "acme",
                  wire = "acme-wire",
                  no_key = true,
                  stream = function() return { role = "assistant", content = {} } end,
                }
              end,
            })
            LLM:register_provider({
              provider = "acme-bad",
              wire = "acme-wire",
              base_url = "https://acme.invalid",
              no_key = true,
              models = { { model_id = "m1", acme_tier = 123 } },
            })
            local bad_st = require("llm.directory").status("acme-bad")
            LLM:register_provider({
              provider = "acme-ok",
              wire = "acme-wire",
              base_url = "https://acme.invalid",
              no_key = true,
              acme_flag = true,
              models = { { model_id = "m1", acme_tier = "pro" } },
            })
            local ok_st = require("llm.directory").status("acme-ok")
            local text = LLM:text_of({
              role = "assistant",
              content = {
                { type = "text", text = "hello " },
                { type = "acme-wire.summary", text = "world", native = { ["acme-wire"] = { k = 1 } } },
                { type = "acme-wire.opaque", native = { ["acme-wire"] = { k = 2 } } },
              },
            })
            local m = LLM:resolve("acme-ok/m1")
            return mare.json.encode({
              bad_state = bad_st and bad_st.state or nil,
              bad_err = bad_st and bad_st.reason or nil,
              ok_state = ok_st and ok_st.state or nil,
              tier = m and m.acme_tier or nil,
              text = text,
            })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();
        assert_eq!(out["bad_state"], "unresolved", "{out}");
        assert!(
            out["bad_err"].as_str().is_some_and(|s| s.contains("acme_tier")),
            "wire model_fields must reject wrong type: {out}"
        );
        assert_eq!(out["ok_state"], "registered", "valid wire model_fields must succeed: {out}");
        assert_eq!(out["tier"], "pro");
        assert_eq!(out["text"], "hello world");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn anthropic_sealed_empty_and_mixed_reasoning_blocks_roundtrip_without_unsigned_leak() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        let ev = |t: &str, d: Value| format!("event: {t}\ndata: {d}\n\n");
        let sse = [
            ev("message_start", json!({"type":"message_start","message":{"usage":{"input_tokens":10}}})),
            // 1) Signed thinking with empty text (display="omitted")
            ev("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}})),
            ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-omitted-1"}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":0})),
            // 2) Signed thinking with text
            ev("content_block_start", json!({"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}})),
            ev("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"visible thought"}})),
            ev("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"sig-visible-2"}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":1})),
            // 3) Redacted thinking
            ev("content_block_start", json!({"type":"content_block_start","index":2,"content_block":{"type":"redacted_thinking","data":"redacted-blob-3"}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":2})),
            ev("content_block_start", json!({"type":"content_block_start","index":3,"content_block":{"type":"text","text":"answer"}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":3})),
            ev("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}})),
            ev("message_stop", json!({"type":"message_stop"})),
        ]
        .concat();
        let ok_sse = [
            ev("message_start", json!({"type":"message_start","message":{"usage":{"input_tokens":5}}})),
            ev("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":0})),
            ev("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}})),
            ev("message_stop", json!({"type":"message_stop"})),
        ]
        .concat();
        server.route("/anthropic", vec![Resp::sse(sse), Resp::sse(ok_sse.clone()), Resp::sse(ok_sse)]);

        let h = provider_host(&mut env, &server, json!({})).await;
        exec(
            &h,
            r#"
            local ant = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local m1 = LLM:stream({
              model = ant,
              system = "S",
              messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            }, function() end)
            -- Replay to same provider: all 3 reasoning blocks stay separate and intact
            LLM:stream({
              model = ant,
              system = "S",
              messages = {
                { role = "user", content = { { type = "text", text = "hi" } } },
                m1,
                { role = "user", content = { { type = "text", text = "next" } } },
              },
            }, function() end)
            -- Foreign unsigned reasoning (no signature) must never be emitted as Anthropic thinking
            LLM:stream({
              model = ant,
              system = "S",
              messages = {
                { role = "user", content = { { type = "text", text = "hi" } } },
                {
                  role = "assistant",
                  provider_id = "other",
                  model_id = "other-1",
                  content = {
                    { type = "reasoning", text = "foreign chain" },
                    { type = "text", text = "foreign reply" },
                  },
                },
                { role = "user", content = { { type = "text", text = "go" } } },
              },
            }, function() end)
            "#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 3);
        let r1_blocks = reqs[1].json()["messages"][1]["content"].as_array().unwrap().clone();
        assert_eq!(r1_blocks.len(), 4, "{r1_blocks:?}");
        assert_eq!(r1_blocks[0], json!({ "type": "thinking", "thinking": "", "signature": "sig-omitted-1" }));
        assert_eq!(r1_blocks[1], json!({ "type": "thinking", "thinking": "visible thought", "signature": "sig-visible-2" }));
        assert_eq!(r1_blocks[2], json!({ "type": "redacted_thinking", "data": "redacted-blob-3" }));
        assert_eq!(r1_blocks[3], json!({ "type": "text", "text": "answer" }));

        let r2_blocks = reqs[2].json()["messages"][1]["content"].as_array().unwrap().clone();
        assert!(
            r2_blocks.iter().all(|b| b["type"] != "thinking"),
            "foreign unsigned reasoning must not be sent as thinking block: {r2_blocks:?}"
        );
        assert!(r2_blocks.iter().any(|b| b["type"] == "text" && b["text"] == "foreign chain"));
        assert!(r2_blocks.iter().any(|b| b["type"] == "text" && b["text"] == "foreign reply"));

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_responses_sealed_reasoning_with_and_without_summary_roundtrips_and_never_synthesizes_foreign() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        let sse = [
            // 1) Sealed reasoning item WITH summary
            r#"data: {"type":"response.reasoning_summary_text.delta","delta":"plan step"}"#,
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_with_sum","type":"reasoning","summary":[{"type":"summary_text","text":"plan step"}],"encrypted_content":"enc-1"}}"#,
            // 2) Sealed reasoning item WITHOUT summary (encrypted_content only)
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"id":"rs_no_sum","type":"reasoning","summary":[],"encrypted_content":"enc-2"}}"#,
            // 3) Assistant text
            r#"data: {"type":"response.output_text.delta","output_index":2,"item_id":"msg_1","content_index":0,"delta":"done"}"#,
            r#"data: {"type":"response.output_item.done","output_index":2,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"done","annotations":[]}]}}"#,
            r#"data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            "",
        ]
        .join("\n\n");
        server.route(
            "/openai",
            vec![Resp::sse(sse), Resp::sse(responses_text("ok1")), Resp::sse(responses_text("ok2"))],
        );

        let h = provider_host(&mut env, &server, json!({})).await;
        let out_json: String = eval(
            &h,
            r#"
            local oa = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local deltas = {}
            local m1 = LLM:stream({
              model = oa,
              system = "S",
              messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            }, function(e) deltas[#deltas + 1] = e.type end)
            -- Same-model replay preserves both sealed reasoning items verbatim
            LLM:stream({
              model = oa,
              system = "S",
              messages = {
                { role = "user", content = { { type = "text", text = "hi" } } },
                m1,
                { role = "user", content = { { type = "text", text = "next" } } },
              },
            }, function() end)
            -- Foreign reasoning (no openai-responses native item) never synthesizes a bare reasoning item
            LLM:stream({
              model = oa,
              system = "S",
              messages = {
                { role = "user", content = { { type = "text", text = "hi" } } },
                {
                  role = "assistant",
                  provider_id = "anthropic",
                  model_id = "claude-sonnet-5-5",
                  content = {
                    { type = "reasoning", text = "claude thought", native = { ["anthropic-messages"] = { signature = "sig-c" } } },
                    { type = "text", text = "claude reply" },
                  },
                },
                { role = "user", content = { { type = "text", text = "go" } } },
              },
            }, function() end)
            return mare.json.encode({ m1 = m1, deltas = deltas })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();
        assert!(out["deltas"].as_array().unwrap().iter().any(|v| v == "reasoning_delta"));
        assert!(!out["deltas"].as_array().unwrap().iter().any(|v| v == "thinking_delta"));

        let content = out["m1"]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "reasoning");
        assert_eq!(content[0]["text"], "plan step");
        assert_eq!(content[0]["native"]["openai-responses"]["item"]["encrypted_content"], "enc-1");
        assert_eq!(content[1]["type"], "reasoning");
        assert_eq!(content[1]["text"], "");
        assert_eq!(content[1]["native"]["openai-responses"]["item"]["encrypted_content"], "enc-2");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 3);
        let r1 = reqs[1].json()["input"].as_array().unwrap().clone();
        assert_eq!(r1[1]["type"], "reasoning");
        assert_eq!(r1[1]["encrypted_content"], "enc-1");
        assert_eq!(r1[2]["type"], "reasoning");
        assert_eq!(r1[2]["encrypted_content"], "enc-2");

        let r2 = reqs[2].json()["input"].as_array().unwrap().clone();
        assert!(
            r2.iter().all(|item| item["type"] != "reasoning"),
            "cross-wire reasoning must not synthesize a Responses reasoning item: {r2:?}"
        );

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn google_thought_signatures_on_calls_and_empty_text_roundtrip_via_native() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        let sse = [
            // Signed reasoning + signed first parallel call + unsigned second parallel call + trailing empty text carrying thoughtSignature
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"gem thought","thought":true,"thoughtSignature":"c2lnMQ=="}]}}]}"#,
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"g1","name":"read","args":{"path":"a"}},"thoughtSignature":"c2lnMg=="},{"functionCall":{"id":"g2","name":"read","args":{"path":"b"}}},{"text":"","thoughtSignature":"c2lnMw=="}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5}}"#,
            "",
        ]
        .join("\n\n");
        let ok_sse = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}\n\n";
        server.route(
            "/google",
            vec![Resp::sse(sse), Resp::sse(ok_sse.to_string()), Resp::sse(ok_sse.to_string())],
        );

        let h = provider_host(&mut env, &server, json!({})).await;
        let out_json: String = eval(
            &h,
            r#"
            local gem = assert(LLM:resolve("google/gemini-3-pro-preview"))
            local deltas = {}
            local m1 = LLM:stream({
              model = gem,
              system = "S",
              messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            }, function(e) deltas[#deltas + 1] = e.type end)
            -- Same-model replay keeps signatures on reasoning, first call, and trailing empty text, leaving second parallel call unsigned
            LLM:stream({
              model = gem,
              system = "S",
              messages = {
                { role = "user", content = { { type = "text", text = "hi" } } },
                m1,
                { role = "tool_result", tool_call_id = "g1", tool_name = "read", content = { { type = "text", text = "A" } } },
                { role = "tool_result", tool_call_id = "g2", tool_name = "read", content = { { type = "text", text = "B" } } },
              },
            }, function() end)
            -- Cross-provider Gemini 3 replay injects skip_thought_signature_validator ONLY on first call
            LLM:stream({
              model = gem,
              system = "S",
              messages = {
                { role = "user", content = { { type = "text", text = "hi" } } },
                {
                  role = "assistant",
                  provider_id = "anthropic",
                  model_id = "claude-sonnet-5-5",
                  content = {
                    { type = "tool_call", id = "c1", name = "read", arguments = { path = "a" } },
                    { type = "tool_call", id = "c2", name = "read", arguments = { path = "b" } },
                  },
                },
                { role = "tool_result", tool_call_id = "c1", tool_name = "read", content = { { type = "text", text = "A" } } },
                { role = "tool_result", tool_call_id = "c2", tool_name = "read", content = { { type = "text", text = "B" } } },
              },
            }, function() end)
            return mare.json.encode({ m1 = m1, deltas = deltas })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();
        assert!(out["deltas"].as_array().unwrap().iter().any(|v| v == "reasoning_delta"));
        assert!(!out["deltas"].as_array().unwrap().iter().any(|v| v == "thinking_delta"));

        let content = out["m1"]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "reasoning");
        assert!(content[0].get("signature").is_none());
        assert_eq!(content[0]["native"]["google-generate"]["thought_signature"], "c2lnMQ==");
        assert_eq!(content[1]["type"], "tool_call");
        assert!(content[1].get("signature").is_none());
        assert_eq!(content[1]["native"]["google-generate"]["thought_signature"], "c2lnMg==");
        assert_eq!(content[2]["type"], "tool_call");
        assert!(content[2].get("native").is_none());
        assert_eq!(content[3]["type"], "text");
        assert_eq!(content[3]["text"], "");
        assert_eq!(content[3]["native"]["google-generate"]["thought_signature"], "c2lnMw==");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 3);
        let r1_parts = reqs[1].json()["contents"][1]["parts"].as_array().unwrap().clone();
        assert_eq!(r1_parts.len(), 4, "{r1_parts:?}");
        assert_eq!(r1_parts[0]["thought"], true);
        assert_eq!(r1_parts[0]["thoughtSignature"], "c2lnMQ==");
        assert_eq!(r1_parts[1]["functionCall"]["id"], "g1");
        assert_eq!(r1_parts[1]["thoughtSignature"], "c2lnMg==");
        assert_eq!(r1_parts[2]["functionCall"]["id"], "g2");
        assert!(r1_parts[2].get("thoughtSignature").is_none());
        assert_eq!(r1_parts[3]["text"], "");
        assert_eq!(r1_parts[3]["thoughtSignature"], "c2lnMw==");

        let r2_parts = reqs[2].json()["contents"][1]["parts"].as_array().unwrap().clone();
        assert_eq!(r2_parts[0]["thoughtSignature"], "skip_thought_signature_validator");
        assert!(r2_parts[1].get("thoughtSignature").is_none());

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_chat_reasoning_details_and_thought_signature_roundtrip_and_strip_on_switch() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENROUTER_API_KEY", "test-or");
        env.set_var("MOONSHOT_API_KEY", "test-kimi");
        let server = MockServer::start().await;

        let or_sse = [
            r#"data: {"id":"or_1","choices":[{"index":0,"delta":{"reasoning":"step A ","reasoning_details":[{"type":"reasoning.text","text":"step A ","signature":"sig-or-1"}]}}]}"#,
            r#"data: {"id":"or_1","choices":[{"index":0,"delta":{"reasoning":"step B","reasoning_details":[{"type":"reasoning.text","text":"step B"}]}}]}"#,
            r#"data: {"id":"or_1","choices":[{"index":0,"delta":{"content":"ans","thought_signature":"venice-msg-sig"}}]}"#,
            r#"data: {"id":"or_1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"tc1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"},"thought_signature":"venice-tc-sig"}]},"finish_reason":"tool_calls"}]}"#,
            "data: [DONE]",
            "",
        ]
        .join("\n\n");
        let ok_sse = "data: {\"id\":\"ok\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        server.route("/openrouter", vec![Resp::sse(or_sse), Resp::sse(ok_sse.to_string())]);
        server.route("/kimi", vec![Resp::sse(ok_sse.to_string())]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "openrouter": { "base_url": format!("{}/openrouter", server.base) },
                    "kimi": { "base_url": format!("{}/kimi", server.base) }
                }
            }),
        )
        .await;
        let out_json: String = eval(
            &h,
            r#"
            local or_m = assert(LLM:resolve("openrouter/anthropic/claude-sonnet-5-5"))
            local kimi_m = assert(LLM:resolve("kimi/kimi-k2-thinking"))
            local deltas = {}
            local m1 = LLM:stream({
              model = or_m,
              system = "S",
              messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            }, function(e) deltas[#deltas + 1] = e.type end)
            local history = {
              { role = "user", content = { { type = "text", text = "hi" } } },
              m1,
              { role = "tool_result", tool_call_id = "tc1", tool_name = "read", content = { { type = "text", text = "done" } } },
            }
            -- Same provider replay preserves reasoning_details, reasoning field, message thought_signature, and tool_call thought_signature
            LLM:stream({ model = or_m, system = "S", messages = history }, function() end)
            -- Switching to Kimi strips openai-chat native so foreign reasoning_details and thought_signature never leak
            LLM:stream({ model = kimi_m, system = "S", messages = history }, function() end)
            return mare.json.encode({ m1 = m1, deltas = deltas })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();
        let deltas: Vec<&str> = out["deltas"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        let first_r = deltas.iter().position(|d| *d == "reasoning_delta").expect("reasoning_delta");
        let first_t = deltas.iter().position(|d| *d == "text_delta").expect("text_delta");
        assert!(first_r < first_t, "incremental reasoning_delta must arrive before text_delta: {deltas:?}");
        assert!(!deltas.contains(&"thinking_delta"));

        assert_eq!(out["m1"]["native"]["openai-chat"]["thought_signature"], "venice-msg-sig");
        let content = out["m1"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3, "no provider_raw block: {content:?}");
        assert_eq!(content[0]["type"], "reasoning");
        assert_eq!(content[0]["text"], "step A step B");
        assert!(content[0].get("reasoning_field").is_none());
        assert!(content[0].get("reasoning_details").is_none());
        assert!(content[0].get("signature").is_none());
        assert_eq!(content[0]["native"]["openai-chat"]["reasoning_field"], "reasoning");
        assert_eq!(content[0]["native"]["openai-chat"]["reasoning_details"][0]["signature"], "sig-or-1");
        assert_eq!(content[2]["type"], "tool_call");
        assert!(content[2].get("signature").is_none());
        assert!(content[2].get("signature_field").is_none());
        assert_eq!(content[2]["native"]["openai-chat"]["thought_signature"], "venice-tc-sig");
        assert_eq!(content[2]["native"]["openai-chat"]["signature_field"], "thought_signature");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 3);
        let or_replay = &reqs[1].json()["messages"][2];
        assert_eq!(or_replay["reasoning"], "step A step B");
        assert_eq!(or_replay["reasoning_details"][0]["signature"], "sig-or-1");
        assert_eq!(or_replay["thought_signature"], "venice-msg-sig");
        assert_eq!(or_replay["tool_calls"][0]["thought_signature"], "venice-tc-sig");

        let kimi_replay = &reqs[2].json()["messages"][2];
        assert!(kimi_replay.get("reasoning_details").is_none(), "foreign reasoning_details must not leak to Kimi: {kimi_replay}");
        assert!(kimi_replay.get("thought_signature").is_none(), "foreign thought_signature must not leak to Kimi: {kimi_replay}");
        assert!(kimi_replay["tool_calls"][0].get("thought_signature").is_none());

        h.shutdown().await;
    })
    .await
}
