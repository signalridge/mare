//! OpenAI Responses native client-executed `tool_search` (`execution: "client"`, `defer_loading`,
//! `tool_search_call` / `tool_search_output`, cache-stable `tools[]`, and cross-wire replay).
//!
//! Reference:
//! - OpenAI Responses Tool Search: https://developers.openai.com/api/docs/guides/tools-tool-search
//! - Codex `codex-rs/tools/src/tool_spec.rs:26-31`
//! - Codex `codex-rs/tools/src/responses_api.rs:21,41,169`
//! - Codex `codex-rs/protocol/src/models.rs:1245-1263,1331-1344,1582-1586,4017-4185`
//! - Codex `codex-rs/protocol/src/openai_models.rs:490`
//! - Codex `codex-rs/core/src/context_manager/normalize.rs:45-98,184-224`

use super::*;
use crate::suite::kernel::write_plugin;

const DEFERRED_ECHO_PLUGIN: &str = r#"return {
  name = "deferred-oai-test",
  inject = { "tools" },
  apply = function(ctx)
    mare._oai_search_prepared = 0
    ctx.tools:register_search_source({
      prepare = function()
        mare._oai_search_prepared = (mare._oai_search_prepared or 0) + 1
      end,
    })
    ctx.tools:register("my_deferred_tool", {
      description = "Fetch a mock record by id",
      exposure = "deferred",
      side_effect = "read",
      parameters = {
        type = "object",
        properties = { id = { type = "string" } },
        required = { "id" },
      },
      execute = function(a)
        return { content = "record:" .. tostring(a.id or "") }
      end,
    })
  end,
}"#;

#[tokio::test]
async fn openai_catalog_marks_gpt54_plus_models_defer_loading_and_codex_models_supports_search_tool() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "models": {
                    "openai/gpt-5.6-luna": { "tool_update": false },
                    "openai/gpt-5.4": {},
                    "openai/gpt-5.5": {},
                    "openai/gpt-5.3": {},
                    "openai/gpt-4.1": {},
                    "openai/o3": {}
                }
            }),
        )
        .await;

        let out_json: String = eval(
            &h,
            r#"
            local codex = require("provider-openai.codex_models")
            local function tu(ref)
              local m = assert(LLM:resolve(ref), "missing " .. ref)
              return m.tool_update == "addition_only"
            end
            local c_yes = codex.entry({ slug = "gpt-5.4", supports_search_tool = true })
            local c_no = codex.entry({ slug = "gpt-5.3", supports_search_tool = false })
            return mare.json.encode({
              terra61 = tu("openai/gpt-6.1-terra"),
              sol61 = tu("openai/gpt-6.1-sol"),
              luna6 = tu("openai/gpt-6-luna"),
              terra56 = tu("openai/gpt-5.6-terra"),
              sol56 = tu("openai/gpt-5.6-sol"),
              luna56_override = tu("openai/gpt-5.6-luna"),
              gpt54 = tu("openai/gpt-5.4"),
              gpt55 = tu("openai/gpt-5.5"),
              gpt53 = tu("openai/gpt-5.3"),
              gpt41 = tu("openai/gpt-4.1"),
              o3 = tu("openai/o3"),
              codex_yes = c_yes and c_yes.tool_update,
              codex_no = c_no and c_no.tool_update,
            })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();

        assert_eq!(out["terra61"], true);
        assert_eq!(out["sol61"], true);
        assert_eq!(out["luna6"], true);
        assert_eq!(out["terra56"], true);
        assert_eq!(out["sol56"], true);
        assert_eq!(out["luna56_override"], false, "user override must win");
        assert_eq!(out["gpt54"], true, "gpt-5.4 must default to tool_update=addition_only");
        assert_eq!(out["gpt55"], true, "gpt-5.5 must default to tool_update=addition_only");
        assert_eq!(out["gpt53"], false, "gpt-5.3 must not default to tool_update=addition_only");
        assert_eq!(out["gpt41"], false, "gpt-4.1 must not default to tool_update=addition_only");
        assert_eq!(out["o3"], false, "o3 must not default to tool_update=addition_only");
        assert_eq!(out["codex_yes"], "addition_only", "codex_models.entry must map supports_search_tool=true");
        assert_eq!(out["codex_no"], false, "codex_models.entry must map supports_search_tool=false");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_responses_client_tool_search_emits_defer_loading_keeps_tools_stable_and_returns_tool_search_output() {
    local(async {
        let mut env = TestEnv::new().await;
        write_plugin(&env, "deferred-oai-test", DEFERRED_ECHO_PLUGIN);
        let server = MockServer::start().await;

        let turn1_sse = [
            responses_event(json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "tsc_1",
                    "type": "tool_search_call",
                    "call_id": "call_search_1",
                    "execution": "client",
                    "status": "in_progress",
                    "arguments": {}
                }
            })),
            responses_event(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "tsc_1",
                    "type": "tool_search_call",
                    "call_id": "call_search_1",
                    "execution": "client",
                    "status": "completed",
                    "arguments": { "query": "mock record" }
                }
            })),
            responses_event(json!({
                "type": "response.completed",
                "response": { "usage": { "input_tokens": 100, "output_tokens": 12 } }
            })),
        ]
        .concat();

        let turn2_sse = [
            responses_event(json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "fc_1",
                    "type": "function_call",
                    "call_id": "call_tool_1",
                    "name": "my_deferred_tool"
                }
            })),
            responses_event(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "fc_1",
                    "type": "function_call",
                    "call_id": "call_tool_1",
                    "name": "my_deferred_tool",
                    "arguments": "{\"id\":\"42\"}"
                }
            })),
            responses_event(json!({
                "type": "response.completed",
                "response": { "usage": { "input_tokens": 130, "output_tokens": 15 } }
            })),
        ]
        .concat();

        server.route(
            "/openai",
            vec![Resp::sse(turn1_sse), Resp::sse(turn2_sse), Resp::sse(responses_text("found record 42"))],
        );

        env.set_var("OPENAI_API_KEY", "test-openai");
        let base = server.base.clone();
        let h = env
            .host()
            .user_plugins()
            .config(json!({
                "model": "openai/gpt-6.1-terra",
                "plugins": {
                    "provider-openai": { "base_url": format!("{base}/openai") },
                    "llm": { "retry_base_ms": 1 },
                },
            }))
            .no_session()
            .build()
            .await;
        global(&h, "T", h.service_table("tools").unwrap());

        let rev_before: i64 = eval(&h, "return T:revision()").await;
        let result = h.prompt("fetch record 42", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let (rev_after, prepared): (i64, i64) = eval(&h, "return T:revision(), mare._oai_search_prepared").await;
        assert_eq!(rev_after, rev_before, "native defer_loading must not bump tools revision");
        assert_eq!(prepared, 1, "mare Lua tool_search must run registered search sources");

        let messages = result["messages"].as_array().unwrap();
        let tool_results: Vec<&Value> = messages.iter().filter(|m| m["role"] == "tool_result").collect();
        assert_eq!(tool_results.len(), 2, "{messages:?}");
        assert_eq!(tool_results[0]["details"]["loaded_tools"], json!(["my_deferred_tool"]));
        assert_eq!(tool_results[1]["content"][0]["text"], "record:42");

        // Inspect all 3 OpenAI requests:
        let reqs = server.requests();
        assert_eq!(reqs.len(), 3, "three OpenAI steps expected");
        let r0 = reqs[0].json();
        let r1 = reqs[1].json();
        let r2 = reqs[2].json();
        let req0_tools = r0["tools"].as_array().unwrap();
        let req1_tools = r1["tools"].as_array().unwrap();
        let req2_tools = r2["tools"].as_array().unwrap();

        // Request 0 declares client-executed `{ type: "tool_search", execution: "client", description, parameters }`,
        // NO function tool named `tool_search`, and `my_deferred_tool` with `defer_loading: true`.
        let ts_decl = req0_tools
            .iter()
            .find(|t| t["type"] == "tool_search")
            .unwrap_or_else(|| panic!("client tool_search must be declared: {req0_tools:?}"));
        assert_eq!(ts_decl["execution"], "client", "{ts_decl}");
        assert!(ts_decl["description"].is_string(), "{ts_decl}");
        assert_eq!(ts_decl["parameters"]["type"], "object", "{ts_decl}");
        assert!(
            !req0_tools.iter().any(|t| t["type"] == "function" && t["name"] == "tool_search"),
            "tool_search must be emitted as type=tool_search, not type=function: {req0_tools:?}"
        );
        let deferred_decl = req0_tools
            .iter()
            .find(|t| t["type"] == "function" && t["name"] == "my_deferred_tool")
            .expect("deferred tool must be declared");
        assert_eq!(deferred_decl["defer_loading"], true, "{deferred_decl}");

        // Native path keeps `instructions` and `tools[]` byte-identical across all 3 steps (prompt cache preserved).
        assert_eq!(
            r1["instructions"], r0["instructions"],
            "system prompt prefix must stay unchanged after tool_search"
        );
        assert_eq!(r2["instructions"], r0["instructions"], "system prompt prefix must stay unchanged after tool call");
        assert_eq!(req1_tools, req0_tools, "tools array must stay unchanged after tool_search");
        assert_eq!(req2_tools, req0_tools, "tools array must stay unchanged after deferred tool call");

        // Request 1 input carries `tool_search_call` (preserving `tsc_1`) + `tool_search_output`
        // with `execution: "client"`, `status: "completed"`, and `tools: [{ ..., name: "my_deferred_tool", defer_loading: true }]`.
        let req1_input = r1["input"].as_array().unwrap();
        let tsc = req1_input
            .iter()
            .find(|it| it["type"] == "tool_search_call")
            .unwrap_or_else(|| panic!("missing tool_search_call in req1: {req1_input:?}"));
        let tso = req1_input
            .iter()
            .find(|it| it["type"] == "tool_search_output")
            .unwrap_or_else(|| panic!("missing tool_search_output in req1: {req1_input:?}"));
        assert_eq!(tsc["id"], "tsc_1", "server tsc_ id must be preserved");
        assert_eq!(tsc["call_id"], "call_search_1");
        assert_eq!(tsc["execution"], "client");
        assert_eq!(tsc["arguments"]["query"], "mock record");
        assert_eq!(tso["call_id"], "call_search_1");
        assert_eq!(tso["execution"], "client");
        assert_eq!(tso["status"], "completed");
        let loaded = tso["tools"].as_array().unwrap();
        assert_eq!(loaded.len(), 1, "{tso}");
        assert_eq!(loaded[0]["type"], "function");
        assert_eq!(loaded[0]["name"], "my_deferred_tool");
        assert_eq!(loaded[0]["defer_loading"], true);
        assert_eq!(loaded[0]["parameters"]["type"], "object");

        // Request 2 preserves the full sequence in order.
        let req2_input = r2["input"].as_array().unwrap();
        let idx_tsc = req2_input.iter().position(|it| it["type"] == "tool_search_call").unwrap();
        let idx_tso = req2_input.iter().position(|it| it["type"] == "tool_search_output").unwrap();
        let idx_fc = req2_input.iter().position(|it| it["type"] == "function_call").unwrap();
        let idx_fco = req2_input.iter().position(|it| it["type"] == "function_call_output").unwrap();
        assert!(idx_tsc < idx_tso && idx_tso < idx_fc && idx_fc < idx_fco, "{req2_input:?}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_responses_client_tool_search_empty_match_and_missing_output_normalization() {
    local(async {
        let mut env = TestEnv::new().await;
        write_plugin(&env, "deferred-oai-test", DEFERRED_ECHO_PLUGIN);
        let server = MockServer::start().await;

        // Turn 1: client tool_search with no matches -> empty `tools: []` in tool_search_output.
        let turn1_sse = [
            responses_event(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "tsc_empty",
                    "type": "tool_search_call",
                    "call_id": "call_empty_1",
                    "execution": "client",
                    "status": "completed",
                    "arguments": { "query": "nonexistent_xyz" }
                }
            })),
            responses_event(json!({
                "type": "response.completed",
                "response": { "usage": { "input_tokens": 50, "output_tokens": 5 } }
            })),
        ]
        .concat();

        server.route("/openai", vec![Resp::sse(turn1_sse), Resp::sse(responses_text("nothing matched"))]);

        env.set_var("OPENAI_API_KEY", "test-openai");
        let base = server.base.clone();
        let h = env
            .host()
            .user_plugins()
            .config(json!({
                "model": "openai/gpt-6.1-terra",
                "plugins": {
                    "provider-openai": { "base_url": format!("{base}/openai") },
                    "llm": { "retry_base_ms": 1 },
                },
            }))
            .no_session()
            .build()
            .await;

        let res = h.prompt("find nonexistent_xyz", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        let r1 = reqs[1].json();
        let tso = r1["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|it| it["type"] == "tool_search_output")
            .unwrap_or_else(|| panic!("expected tool_search_output: {r1}"));
        assert_eq!(tso["call_id"], "call_empty_1");
        assert_eq!(tso["execution"], "client");
        assert_eq!(tso["status"], "completed");
        assert_eq!(tso["tools"], json!([]), "empty match must serialize as JSON array []");

        // Normalization of an orphan client tool_search_call without output synthesizes tool_search_output.
        let norm_json: String = eval(
            &h,
            r#"
            local wire = require("provider-openai.wire")
            local items = wire.normalize({
              { type = "tool_search_call", id = "tsc_aborted", call_id = "call_aborted", execution = "client", arguments = { query = "foo" } },
              { type = "tool_search_output", call_id = "call_orphan", execution = "client", status = "completed", tools = mare.json.array() },
            })
            return mare.json.encode(items)
            "#,
        )
        .await;
        let norm: Value = serde_json::from_str(&norm_json).unwrap();
        let arr = norm.as_array().unwrap();
        assert_eq!(arr.len(), 2, "orphan output dropped and missing output synthesized: {arr:?}");
        assert_eq!(arr[0]["type"], "tool_search_call");
        assert_eq!(arr[0]["id"], "tsc_aborted");
        assert_eq!(arr[1]["type"], "tool_search_output");
        assert_eq!(arr[1]["call_id"], "call_aborted");
        assert_eq!(arr[1]["execution"], "client");
        assert_eq!(arr[1]["status"], "completed");
        assert_eq!(arr[1]["tools"], json!([]));

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_responses_tool_search_cross_wire_replay_and_server_hosted_items() {
    local(async {
        let mut env = TestEnv::new().await;
        write_plugin(&env, "deferred-oai-test", DEFERRED_ECHO_PLUGIN);
        let server = MockServer::start().await;

        let ant_ok_sse = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"ant ok\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]
        .concat();
        let google_ok_sse = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"gem ok\"}]},\"finishReason\":\"STOP\"}]}\n\n";
        let compat_ok_sse = compat_events(&[compat_text("compat ok"), compat_stop()]);

        // Also exercise server-executed tool_search_call/output (hosted=true, same-wire replay, cross-wire drop).
        let server_ts_sse = [
            responses_event(json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "tsc_srv_1",
                    "type": "tool_search_call",
                    "execution": "server",
                    "call_id": null,
                    "status": "in_progress",
                    "arguments": { "paths": ["crm"] }
                }
            })),
            responses_event(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "tsc_srv_1",
                    "type": "tool_search_call",
                    "execution": "server",
                    "call_id": null,
                    "status": "completed",
                    "arguments": { "paths": ["crm"] }
                }
            })),
            responses_event(json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": {
                    "id": "tso_srv_1",
                    "type": "tool_search_output",
                    "execution": "server",
                    "call_id": null,
                    "status": "completed",
                    "tools": []
                }
            })),
            responses_event(json!({
                "type": "response.output_item.done",
                "output_index": 2,
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "srv done" }]
                }
            })),
            responses_event(json!({
                "type": "response.completed",
                "response": { "usage": { "input_tokens": 30, "output_tokens": 5 } }
            })),
        ]
        .concat();

        server.route("/openai", vec![Resp::sse(server_ts_sse), Resp::sse(responses_text("oai replay"))]);
        server.route("/anthropic", vec![Resp::sse(ant_ok_sse.clone()), Resp::sse(ant_ok_sse)]);
        server.route("/google", vec![Resp::sse(google_ok_sse.to_string())]);
        server.route("/compat", vec![Resp::sse(compat_ok_sse)]);

        let h = provider_host(&mut env, &server, json!({})).await;
        let out_json: String = eval(
            &h,
            r#"
            local oai = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local ant = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local gem = assert(LLM:resolve("google/gemini-3-pro-preview"))
            local ds = assert(LLM:resolve("compat/chat-model"))

            -- 1. Client-executed tool_search history from OpenAI replayed to Anthropic yields tool_reference.
            local client_history = {
              { role = "user", content = { { type = "text", text = "find my_deferred_tool" } } },
              {
                role = "assistant",
                provider_id = "openai",
                model_id = "gpt-6.1-terra",
                content = {
                  {
                    type = "tool_call",
                    id = "call_s1",
                    name = "tool_search",
                    arguments = { query = "my_deferred_tool" },
                    native = { ["openai-responses"] = { item = { id = "tsc_1", type = "tool_search_call", call_id = "call_s1", execution = "client", arguments = { query = "my_deferred_tool" } } } },
                  },
                },
              },
              {
                role = "tool_result",
                tool_call_id = "call_s1",
                tool_name = "tool_search",
                content = { { type = "text", text = "Loaded my_deferred_tool" } },
                details = { loaded_tools = { "my_deferred_tool" } },
              },
            }
            local ant_tools = {
              { name = "tool_search", description = "search", parameters = { type = "object", properties = { query = { type = "string" } } } },
              { name = "my_deferred_tool", description = "mock", defer_loading = true, parameters = { type = "object", properties = {} } },
            }
            LLM:stream({ model = ant, system = "S", messages = client_history, tools = ant_tools }, function() end)

            -- 2. Server-executed tool_search_call/output stored as hosted=true, replayed same-wire only, dropped cross-wire.
            local srv_events = {}
            local srv_msg = LLM:stream({
              model = oai,
              system = "S",
              messages = { { role = "user", content = { { type = "text", text = "check crm" } } } },
            }, function(e) srv_events[#srv_events + 1] = e end)
            local srv_history = {
              { role = "user", content = { { type = "text", text = "check crm" } } },
              srv_msg,
              { role = "user", content = { { type = "text", text = "next" } } },
            }
            LLM:stream({ model = oai, system = "S", messages = srv_history }, function() end)
            LLM:stream({ model = ant, system = "S", messages = srv_history }, function() end)
            LLM:stream({ model = gem, system = "S", messages = srv_history }, function() end)
            LLM:stream({ model = ds, system = "S", messages = srv_history }, function() end)
            return mare.json.encode({ srv_msg = srv_msg, srv_events = srv_events })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();

        assert_eq!(out["srv_msg"]["stop_reason"], "stop");
        assert_eq!(out["srv_msg"]["content"][0]["hosted"], true);
        assert_eq!(out["srv_msg"]["content"][0]["native"]["openai-responses"]["item"]["type"], "tool_search_call");
        assert_eq!(
            out["srv_msg"]["content"][0]["native"]["openai-responses"]["output_item"]["type"],
            "tool_search_output"
        );
        let ev_types: Vec<&str> =
            out["srv_events"].as_array().unwrap().iter().filter_map(|e| e["type"].as_str()).collect();
        assert_eq!(ev_types, vec!["hosted_tool_start", "hosted_tool_done"], "{ev_types:?}");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 6);

        // reqs[0]: Anthropic cross-wire replay of client tool_search emits tool_use + tool_reference
        let r0 = reqs[0].json();
        assert_eq!(r0["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(r0["messages"][1]["content"][0]["name"], "tool_search");
        assert_eq!(r0["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(r0["messages"][2]["content"][0]["content"][0]["type"], "tool_reference");
        assert_eq!(r0["messages"][2]["content"][0]["content"][0]["tool_name"], "my_deferred_tool");

        // reqs[2]: OpenAI same-wire replay of server hosted tool_search keeps tsc_srv_1 + tso_srv_1
        let r2 = reqs[2].json();
        let r2_input = r2["input"].as_array().unwrap();
        assert!(r2_input.iter().any(|it| it["type"] == "tool_search_call" && it["id"] == "tsc_srv_1"));
        assert!(r2_input.iter().any(|it| it["type"] == "tool_search_output" && it["id"] == "tso_srv_1"));

        // reqs[3..5]: Anthropic / Google / DeepSeek drop server hosted tool_search
        let r3 = reqs[3].json();
        assert_eq!(r3["messages"][1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(r3["messages"][1]["content"][0]["text"], "srv done");

        let r4 = reqs[4].json();
        let gem_parts = r4["contents"][1]["parts"].as_array().unwrap();
        assert!(gem_parts.iter().all(|p| p.get("functionCall").is_none()));

        let r5 = reqs[5].json();
        assert!(r5["messages"][2].get("tool_calls").is_none());

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn non_native_model_keeps_function_tool_search_without_defer_loading() {
    local(async {
        let mut env = TestEnv::new().await;
        write_plugin(&env, "deferred-oai-test", DEFERRED_ECHO_PLUGIN);
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(responses_text("ok"))]);

        env.set_var("OPENAI_API_KEY", "test-openai");
        let base = server.base.clone();
        let h = env
            .host()
            .user_plugins()
            .config(json!({
                "model": "openai/gpt-5.3",
                "models": { "openai/gpt-5.3": {} },
                "plugins": {
                    "provider-openai": { "base_url": format!("{base}/openai") },
                    "llm": { "retry_base_ms": 1 },
                },
            }))
            .no_session()
            .build()
            .await;

        let res = h.prompt("hello", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        let r0 = reqs[0].json();
        let tools = r0["tools"].as_array().unwrap();
        assert!(!tools.iter().any(|t| t["type"] == "tool_search"), "gpt-5.3 must not emit type=tool_search: {tools:?}");
        assert!(
            tools.iter().any(|t| t["type"] == "function" && t["name"] == "tool_search"),
            "gpt-5.3 must keep client function tool_search: {tools:?}"
        );
        assert!(
            !tools.iter().any(|t| t["name"] == "my_deferred_tool"),
            "unloaded deferred tool must stay hidden on non-native model: {tools:?}"
        );

        h.shutdown().await;
    })
    .await
}
