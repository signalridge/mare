//! Signatures and sealed reasoning go from the stream into the next request:
//! the request carrying a tool result replays, verbatim and before the call they belong
//! to, what the model sealed while making that call — Anthropic's thinking signature (assembled
//! from `signature_delta` pieces) and `redacted_thinking` data, Google's `thoughtSignature` on
//! the `functionCall`, a Responses reasoning item's `encrypted_content`. The existing checks
//! cover each half alone: protocol_quirks.rs encodes hand-written history, wires.rs decodes.

use super::reasoning::ROUND_TRIP;
use super::*;

/// Anthropic: a thinking block whose signature arrives in two `signature_delta` pieces, a
/// `redacted_thinking` block, then the `read` call.
fn anthropic_sealed_call() -> String {
    [
        json!({ "type": "message_start", "message": { "id": "msg_s", "role": "assistant", "content": [],
            "usage": { "input_tokens": 10, "output_tokens": 1 } } }),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": "Need the file." } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "SIG-PART-A" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "SIG-PART-B" } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "redacted_thinking", "data": "REDACTED-DATA" } }),
        json!({ "type": "content_block_stop", "index": 1 }),
        json!({ "type": "content_block_start", "index": 2, "content_block": { "type": "tool_use", "id": "toolu_s", "name": "read", "input": {} } }),
        json!({ "type": "content_block_delta", "index": 2, "delta": { "type": "input_json_delta", "partial_json": "{\"path\":\"src/main.rs\"}" } }),
        json!({ "type": "content_block_stop", "index": 2 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 30 } }),
        json!({ "type": "message_stop" }),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

fn anthropic_text(text: &str) -> String {
    [
        json!({ "type": "message_start", "message": { "id": "msg_t", "role": "assistant", "content": [],
            "usage": { "input_tokens": 10, "output_tokens": 1 } } }),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 1 } }),
        json!({ "type": "message_stop" }),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

fn position(items: &[Value], what: &str, pred: impl Fn(&Value) -> bool) -> Result<usize, String> {
    items.iter().position(pred).ok_or_else(|| format!("no {what} in {}", Value::Array(items.to_vec())))
}

/// The second request of a tool round per wire, checked exactly: the sealed parts with their
/// text, the call with its name and arguments (once), and the one tool result that answers it
/// after the call. Each check names what is missing or misplaced. All wires run before the
/// verdict.
#[tokio::test]
async fn sealed_reasoning_from_the_stream_is_replayed_before_its_call() {
    let mut problems = Vec::new();
    for wire in ["anthropic", "google", "responses"] {
        local(async {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let (route, model) = match wire {
                "anthropic" => {
                    server.route("/anthropic", vec![Resp::sse(anthropic_sealed_call()), Resp::sse(anthropic_text("done"))]);
                    ("/anthropic", "anthropic/claude-sonnet-5-5")
                }
                "google" => {
                    let done = format!(
                        "data: {}\n\n",
                        json!({ "candidates": [{ "content": { "role": "model", "parts": [{ "text": "done" }] }, "finishReason": "STOP" }] })
                    );
                    server.route("/google", vec![Resp::sse(fixture("google.sse")), Resp::sse(done)]);
                    ("/google", "google/gemini-2.5-pro")
                }
                _ => {
                    server.route("/openai", vec![Resp::sse(fixture("openai-responses.sse")), Resp::sse(responses_text("done"))]);
                    ("/openai", "openai/gpt-6.1-terra")
                }
            };
            let h = provider_host(&mut env, &server, json!({ "plugins": { "provider-openai": { "transport": "http" } } })).await;
            h.lua().load(ROUND_TRIP).into_function().unwrap().call_async::<()>(model).await.unwrap();
            h.shutdown().await;
            let reqs = server.requests();
            if reqs.len() != 2 || server.count(route) != 2 {
                problems.push(format!("{wire}: {} requests", reqs.len()));
                return;
            }
            let req = reqs[1].json();
            let check = || -> Result<(), String> {
                let args = json!({ "path": "src/main.rs" });
                let parse = |v: &Value| match v {
                    Value::String(raw) => serde_json::from_str(raw).unwrap_or(Value::Null),
                    other => other.clone(),
                };
                let only = |items: &[Value], what: &str, pred: &dyn Fn(&Value) -> bool| -> Result<usize, String> {
                    let hits: Vec<usize> = (0..items.len()).filter(|&i| pred(&items[i])).collect();
                    match hits[..] {
                        [i] => Ok(i),
                        _ => Err(format!("{} {what} (want exactly 1) in {}", hits.len(), Value::Array(items.to_vec()))),
                    }
                };
                match wire {
                    "anthropic" => {
                        let msgs = req["messages"].as_array().cloned().unwrap_or_default();
                        let a = only(&msgs, "assistant message", &|m| m["role"] == "assistant")?;
                        let content = msgs[a]["content"].as_array().cloned().unwrap_or_default();
                        let thinking = position(&content, "the thinking block, its text and assembled signature", |b| {
                            b["type"] == "thinking"
                                && b["thinking"] == "Need the file."
                                && b["signature"] == "SIG-PART-ASIG-PART-B"
                        })?;
                        let redacted = position(&content, "redacted_thinking", |b| {
                            b["type"] == "redacted_thinking" && b["data"] == "REDACTED-DATA"
                        })?;
                        let call = only(&content, "tool_use toolu_s read(src/main.rs)", &|b| {
                            b["type"] == "tool_use" && b["id"] == "toolu_s" && b["name"] == "read" && b["input"] == args
                        })?;
                        if !(thinking < redacted && redacted < call) {
                            return Err(format!("order thinking {thinking} / redacted {redacted} / call {call}"));
                        }
                        // the tool result answers that call, in the message right after it
                        let next = msgs.get(a + 1).and_then(|m| m["content"].as_array().cloned()).unwrap_or_default();
                        let blocks: Vec<Value> = msgs.iter().flat_map(|m| m["content"].as_array().cloned().unwrap_or_default()).collect();
                        only(&blocks, "tool_result", &|b| b["type"] == "tool_result")?;
                        only(&next, "tool_result for toolu_s with the file", &|b| {
                            b["type"] == "tool_result" && b["tool_use_id"] == "toolu_s" && b["content"].to_string().contains("fn main() {}")
                        })?;
                    }
                    "google" => {
                        let contents = req["contents"].as_array().cloned().unwrap_or_default();
                        let parts: Vec<Value> = contents.iter().flat_map(|c| c["parts"].as_array().cloned().unwrap_or_default()).collect();
                        let call = only(&parts, "functionCall", &|p| p.get("functionCall").is_some())?;
                        let fc = &parts[call];
                        if fc["functionCall"]["name"] != "read" || fc["functionCall"]["args"] != args || fc["thoughtSignature"] != "c2lnMTIz" {
                            return Err(format!("the functionCall is not read(src/main.rs) with its thoughtSignature: {fc}"));
                        }
                        let result = only(&parts, "functionResponse", &|p| p.get("functionResponse").is_some())?;
                        let fr = &parts[result]["functionResponse"];
                        if result < call || fr["name"] != "read" || !fr["response"].to_string().contains("fn main() {}") {
                            return Err(format!("no matching functionResponse after the call: {}", parts[result]));
                        }
                    }
                    _ => {
                        let input = req["input"].as_array().cloned().unwrap_or_default();
                        let reasoning = position(&input, "reasoning item with its encrypted_content", |i| {
                            i["type"] == "reasoning" && i["encrypted_content"] == "enc123"
                        })?;
                        let call = only(&input, "function_call call_abc read(src/main.rs)", &|i| {
                            i["type"] == "function_call" && i["call_id"] == "call_abc" && i["name"] == "read" && parse(&i["arguments"]) == args
                        })?;
                        if reasoning > call {
                            return Err(format!("reasoning {reasoning} after its call {call}"));
                        }
                        let output = only(&input, "function_call_output", &|i| i["type"] == "function_call_output")?;
                        if output < call || input[output]["call_id"] != "call_abc" || !input[output]["output"].to_string().contains("fn main() {}") {
                            return Err(format!("no matching function_call_output after the call: {}", input[output]));
                        }
                    }
                }
                Ok(())
            };
            if let Err(e) = check() {
                problems.push(format!("{wire}: {e}"));
            }
        })
        .await;
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
