//! Providers against a scripted HTTP server (no tokens burned; after pi `*-sse-parsing` /
//! `*-compat` and Codex mock Responses tests): SSE replay per wire protocol, request body shape,
//! retry / no-retry by status, retry-after capping and a stream cut mid-message.

use std::sync::Arc;

use crate::common::*;
use mare_host::Host;
use serde_json::{Value, json};

mod agent_turns;
mod anthropic_cache_usage;
mod auth;
mod cache_retention;
mod catalog;
mod catalog_query_and_docs;
mod codex_retry;
mod dsh_recovery;
mod errors;
mod finish_and_usage;
mod golden_wire;
mod interrupted_call;
mod kimi_loop;
mod markup_healing;
mod model_resolve;
mod models;
mod native_metadata;
mod native_metadata_edges;
mod openai;
mod openai_tool_search;
mod opencode_cassettes;
mod opencode_go;
mod opencode_recordings;
mod openrouter_cache;
mod params;
mod pi_port;
mod protocol_quirks;
mod prov_misc;
mod provider_anomalies;
mod provider_recordings;
mod reasoning;
mod request_shape;
mod retry;
mod routes;
mod schema_sanitize;
mod scripted_model;
mod signature_roundtrip;
mod sm_codex_compact_retry;
mod streaming;
mod terminal_event;
mod thinking_switches;
mod tool_loading;
mod tool_result_images;
mod transport;
mod vendor_tool_search;
mod vercel_opencode_port;
mod wires;

/// Replays one request through `llm:stream` with a tool-call history (exercises tool results).
const STREAM: &str = r#"
local ref = ...
local m = assert(LLM:resolve(ref), "unknown model " .. ref)
local history = {
  { role = "user", content = { { type = "text", text = "hi" } } },
  { role = "assistant", provider = "other", content = {
      { type = "text", text = "ok" },
      { type = "tool_call", id = "call_0", name = "read", arguments = { path = "a.txt" } } } },
  { role = "tool_result", tool_call_id = "call_0", tool_name = "read", content = { { type = "text", text = "FILE-A-CONTENT" } } },
  { role = "user", content = { { type = "text", text = "now read main" } } },
}
local events = {}
local msg = LLM:stream({
  model = m, system = "SYSTEM-PROMPT", messages = history, effort = "high",
  turn_id = "turn-1", step_id = "step-1",
  tools = { { name = "read", description = "Read a file", parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } } } },
}, function(ev) events[#events + 1] = ev.type end)
msg.events = events
return msg
"#;

async fn provider_host(env: &mut TestEnv, server: &MockServer, extra: Value) -> Arc<Host> {
    for (k, v) in [
        ("OPENAI_API_KEY", "test-openai"),
        ("ANTHROPIC_API_KEY", "test-anthropic"),
        ("DEEPSEEK_API_KEY", "test-deepseek"),
        ("COMPAT_API_KEY", "test-compat"),
        ("GEMINI_API_KEY", "test-google"),
    ] {
        env.set_var(k, v);
    }
    let base = &server.base;
    let h = env
        .host()
        // `"jobs"` avoids `shell_tools.lua:220-226` re-registering `bash` on `app/ready` and dropping `user-config` shadow (tracked in closeout).
        .builtins(&["tools-apply-patch", "mcp", "usage", "compact", "user-config", "jobs"])
        .config(json!({
            "providers": {
                "deepseek": { "base_url": format!("{base}/compat") },
                "compat": {
                    "wire": "openai-chat",
                    "base_url": format!("{base}/compat"),
                    "api_key_env": "COMPAT_API_KEY",
                    "list_models": false,
                    "models": [
                        { "model_id": "chat-model", "context_window": 128000, "max_output": 8192 },
                        { "model_id": "reasoner-model", "context_window": 128000, "max_output": 8192, "reasoning": true, "images": true },
                    ],
                },
            },
            "plugins": {
                "provider-openai": { "base_url": format!("{base}/openai") },
                "provider-anthropic": { "base_url": format!("{base}/anthropic") },
                "provider-google": { "base_url": format!("{base}/google") },
                "llm": { "retry_base_ms": 1 },
            },
        }))
        .config(extra)
        .no_session()
        .build()
        .await;
    global(&h, "LLM", h.service_table("llm").unwrap());
    h
}

async fn stream(h: &Host, model: &str) -> Value {
    let f = h.lua().load(STREAM).into_function().unwrap();
    let v: mlua::Value = f.call_async(model).await.unwrap_or_else(|e| panic!("{model}: {e}"));
    mare_shared::lua::lua_to_json(h.lua(), v).unwrap()
}

/// The shared fixture shape: thinking + "Reading it now." + read(src/main.rs), usage 100/45/20.
fn assert_fixture_message(model: &str, msg: &Value, cache_write: i64) {
    assert!(msg.get("error").is_none_or(Value::is_null), "{model}: {msg}");
    let content = msg["content"].as_array().unwrap();
    let find = |t: &str| content.iter().find(|b| b["type"] == t).unwrap_or_else(|| panic!("{model}: no {t} in {msg}"));
    assert_eq!(find("text")["text"], "Reading it now.", "{model}");
    assert_eq!(find("reasoning")["text"].as_str().unwrap().trim(), "Need to read the file.", "{model}");
    assert_eq!(find("tool_call")["arguments"]["path"], "src/main.rs", "{model}");
    assert_eq!(msg["stop_reason"], "tool_use", "{model}");
    let u = &msg["usage"];
    assert_eq!(
        (u["input"].as_i64(), u["output"].as_i64(), u["cache_read"].as_i64()),
        (Some(100), Some(45), Some(20)),
        "{model}: {u}"
    );
    assert_eq!(u["cache_write"], cache_write, "{model}");
    let events: Vec<&str> = msg["events"].as_array().unwrap().iter().filter_map(|e| e.as_str()).collect();
    for e in ["text_delta", "reasoning_delta", "tool_call_start"] {
        assert!(events.contains(&e), "{model}: missing {e} in {events:?}");
    }
}

/// One Responses request with a session id (headers are per session), returning the result or
/// the error text.
async fn openai_call(h: &Host) -> Result<Value, String> {
    let code = r#"
local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
local ok, res = pcall(function()
  return LLM:stream({ model = m, system = "S", session_id = "sess-1", purpose = "agent",
    messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
end)
if ok and res.stop_reason ~= "error" then return res end
if ok then error(res.error) end
error(type(res) == "table" and (res.message or mare.json.encode(res)) or tostring(res))
"#;
    let f = h.lua().load(code).into_function().unwrap();
    match f.call_async::<mlua::Value>(()).await {
        Ok(v) => Ok(mare_shared::lua::lua_to_json(h.lua(), v).unwrap()),
        Err(e) => Err(e.to_string()),
    }
}

/// One SSE event of the Responses API.
fn responses_event(data: Value) -> String {
    format!("event: {}\ndata: {data}\n\n", data["type"].as_str().unwrap())
}

/// A plain completed text answer (Responses API).
fn responses_text(text: &str) -> String {
    responses_event(json!({ "type": "response.output_item.done", "item": {
        "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": text }] } }))
        + &responses_event(json!({ "type": "response.completed", "response": { "usage": {} } }))
}

fn compat_events(events: &[Value]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}

// ---- batch P: streaming edge cases, mid-stream errors, usage, crash guard ---------------------

/// One plain request (no history, no tools) returning the message.
async fn ask(h: &Host, model: &str) -> Value {
    let v: mlua::Value = h
        .lua()
        .load(
            r#"local ref = ...
local m = assert(LLM:resolve(ref))
return LLM:stream({ model = m, messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)"#,
        )
        .into_function()
        .unwrap()
        .call_async::<mlua::Value>(model)
        .await
        .unwrap_or_else(|e| panic!("{model}: {e}"));
    mare_shared::lua::lua_to_json(h.lua(), v).unwrap()
}

/// A synthetic provider protocol fixture under `tests/fixtures/providers/`.
fn provider_fixture(rel: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/providers").join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn text_of(msg: &Value) -> String {
    msg["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "text")
        .map(|b| b["text"].as_str().unwrap())
        .collect()
}

fn compat_text(text: &str) -> Value {
    json!({"choices":[{"index":0,"delta":{"content":text}}]})
}

fn compat_stop() -> Value {
    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
}

/// Records what every provider passes to `mare.http.request` for its main request (timeouts,
/// `fresh`) — the wrapper calls through, so the request still reaches the mock server.
const SPY_HTTP: &str = r#"
SEEN = {}
local orig = mare.http.request
mare.http.request = function(o)
  if type(o) == "table" and o.method == "POST" then
    SEEN[#SEEN + 1] = { idle = o.idle_timeout_ms, header = o.header_timeout_ms, fresh = o.fresh == true, url = o.url }
  end
  return orig(o)
end
"#;
