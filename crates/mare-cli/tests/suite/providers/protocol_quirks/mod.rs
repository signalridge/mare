//! Protocol quirks across the four built-in wires:
//! 1. Thinking signatures: replay signed/redacted blocks to the same provider/model, never
//!    fabricate empty signatures, and strip foreign signatures when switching providers.
//! 2. Tool call IDs: deduplicate repeated IDs across turns, deterministically synthesize missing
//!    IDs (stable across replays), and sanitize/truncate to each wire's format and length limit.
//! 3. Encrypted reasoning content (Responses `encrypted_content`): replay only to the same
//!    provider and model; drop when switching model/provider; keep summary-only when absent.
//! 4. `Retry-After` capping (`llm.retry_after_max_ms`): fail immediately without sleeping when the
//!    server asks for longer than the configured cap.

use std::time::{Duration, Instant};

use super::*;

/// Runs `LLM:stream` with a JSON-encoded `messages` array and optional `tools` array.
pub(super) async fn stream_with_history(h: &Host, model: &str, messages: Value, tools: Option<Value>) -> Value {
    let code = r#"
local ref, msgs_json, tools_json = ...
local m = assert(LLM:resolve(ref), "unknown model " .. ref)
local msgs = mare.json.decode(msgs_json)
local tools = tools_json and mare.json.decode(tools_json) or nil
return LLM:stream({
  model = m,
  system = "SYSTEM-PROMPT",
  messages = msgs,
  tools = tools,
  effort = "high",
  no_fallback = true,
}, function() end)
"#;
    let f = h.lua().load(code).into_function().unwrap();
    let tools_str = tools.map(|t| t.to_string());
    let v: mlua::Value =
        f.call_async((model, messages.to_string(), tools_str)).await.unwrap_or_else(|e| panic!("{model}: {e}"));
    mare_shared::lua::lua_to_json(h.lua(), v).unwrap()
}

pub(super) fn read_tool_decl() -> Value {
    json!([{
        "name": "read",
        "description": "Read a file",
        "parameters": {
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }
    }])
}

mod fixtures;
mod history;
mod retry_and_wire;
