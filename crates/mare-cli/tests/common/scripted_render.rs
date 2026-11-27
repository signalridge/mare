//! SSE and HTTP response renderers for `common::scripted`.

use serde_json::{Value, json};

use super::Resp;
use super::scripted::{Finish, Part, Step, Wire};

pub fn render(wire: Wire, parts: &[Part]) -> Vec<String> {
    let mut finish = None;
    let mut usage = (0u64, 0u64, 0u64);
    let mut has_tool = false;
    let mut has_partial = false;
    for p in parts {
        match p {
            Part::ToolCall { .. } | Part::SignedToolCall { .. } => has_tool = true,
            Part::PartialToolCall { .. } => has_partial = true,
            Part::Finish(f) => finish = Some(*f),
            Part::Usage { input, output, cached } => usage = (*input, *output, *cached),
            _ => {}
        }
    }
    let finish = finish.unwrap_or(if has_tool { Finish::ToolCalls } else { Finish::Stop });
    match wire {
        Wire::Responses => render_responses(parts, finish, usage, has_partial),
        Wire::Anthropic => render_anthropic(parts, finish, usage, has_partial),
        Wire::Compat => render_compat(parts, finish, usage, has_partial),
        Wire::Gemini => render_gemini(parts, finish, usage, has_partial),
    }
}

fn sse_named(ty: &str, data: Value) -> String {
    format!("event: {ty}\ndata: {data}\n\n")
}

fn sse_data(data: Value) -> String {
    format!("data: {data}\n\n")
}

fn render_responses(parts: &[Part], finish: Finish, usage: (u64, u64, u64), partial: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut seq = 0usize;
    let mut push = |ty: &str, mut data: Value| {
        data["type"] = json!(ty);
        data["sequence_number"] = json!(seq);
        seq += 1;
        out.push(sse_named(ty, data));
    };
    push("response.created", json!({ "response": { "id": "resp_1", "object": "response", "status": "in_progress" } }));
    for (idx, p) in parts.iter().enumerate() {
        match p {
            Part::Reasoning { text, signature } => {
                let id = format!("rs_{idx}");
                push(
                    "response.output_item.added",
                    json!({ "output_index": idx, "item": { "id": id, "type": "reasoning", "status": "in_progress", "summary": [] } }),
                );
                push(
                    "response.reasoning_summary_part.added",
                    json!({ "item_id": id, "output_index": idx, "summary_index": 0, "part": { "type": "summary_text", "text": "" } }),
                );
                push(
                    "response.reasoning_summary_text.delta",
                    json!({ "item_id": id, "output_index": idx, "summary_index": 0, "delta": text }),
                );
                push(
                    "response.reasoning_summary_text.done",
                    json!({ "item_id": id, "output_index": idx, "summary_index": 0, "text": text }),
                );
                push(
                    "response.reasoning_summary_part.done",
                    json!({ "item_id": id, "output_index": idx, "summary_index": 0, "part": { "type": "summary_text", "text": text } }),
                );
                let mut item = json!({ "id": id, "type": "reasoning", "status": "completed", "summary": [{ "type": "summary_text", "text": text }] });
                if let Some(sig) = signature {
                    item["encrypted_content"] = json!(sig);
                }
                push("response.output_item.done", json!({ "output_index": idx, "item": item }));
            }
            Part::Text(text) => {
                let id = format!("msg_{idx}");
                push(
                    "response.output_item.added",
                    json!({ "output_index": idx, "item": { "id": id, "type": "message", "role": "assistant", "status": "in_progress", "content": [] } }),
                );
                push(
                    "response.content_part.added",
                    json!({ "item_id": id, "output_index": idx, "content_index": 0, "part": { "type": "output_text", "text": "", "annotations": [] } }),
                );
                push(
                    "response.output_text.delta",
                    json!({ "item_id": id, "output_index": idx, "content_index": 0, "delta": text }),
                );
                push(
                    "response.output_text.done",
                    json!({ "item_id": id, "output_index": idx, "content_index": 0, "text": text }),
                );
                push(
                    "response.content_part.done",
                    json!({ "item_id": id, "output_index": idx, "content_index": 0, "part": { "type": "output_text", "text": text, "annotations": [] } }),
                );
                push(
                    "response.output_item.done",
                    json!({ "output_index": idx, "item": { "id": id, "type": "message", "role": "assistant", "status": "completed", "content": [{ "type": "output_text", "text": text, "annotations": [] }] } }),
                );
            }
            Part::ToolCall { id, name, args } | Part::SignedToolCall { id, name, args, .. } => {
                let item_id = format!("fc_{idx}");
                let raw = args.to_string();
                push(
                    "response.output_item.added",
                    json!({ "output_index": idx, "item": { "id": item_id, "type": "function_call", "call_id": id, "name": name, "arguments": "", "status": "in_progress" } }),
                );
                push(
                    "response.function_call_arguments.delta",
                    json!({ "item_id": item_id, "output_index": idx, "delta": raw }),
                );
                push(
                    "response.function_call_arguments.done",
                    json!({ "item_id": item_id, "output_index": idx, "name": name, "arguments": raw }),
                );
                push(
                    "response.output_item.done",
                    json!({ "output_index": idx, "item": { "id": item_id, "type": "function_call", "call_id": id, "name": name, "arguments": raw, "status": "completed" } }),
                );
            }
            Part::PartialToolCall { id, name, raw_args } => {
                let item_id = format!("fc_{idx}");
                push(
                    "response.output_item.added",
                    json!({ "output_index": idx, "item": { "id": item_id, "type": "function_call", "call_id": id, "name": name, "arguments": "", "status": "in_progress" } }),
                );
                push(
                    "response.function_call_arguments.delta",
                    json!({ "item_id": item_id, "output_index": idx, "delta": raw_args }),
                );
            }
            Part::Finish(_) | Part::Usage { .. } => {}
        }
    }
    if !partial {
        let u = json!({ "input_tokens": usage.0, "output_tokens": usage.1, "input_tokens_details": { "cached_tokens": usage.2 } });
        match finish {
            Finish::Length => push(
                "response.incomplete",
                json!({ "response": { "id": "resp_1", "status": "incomplete", "incomplete_details": { "reason": "max_output_tokens" }, "usage": u } }),
            ),
            Finish::ContentFilter => push(
                "response.incomplete",
                json!({ "response": { "id": "resp_1", "status": "incomplete", "incomplete_details": { "reason": "content_filter" }, "usage": u } }),
            ),
            Finish::Stop | Finish::ToolCalls => {
                push("response.completed", json!({ "response": { "id": "resp_1", "status": "completed", "usage": u } }))
            }
        }
    }
    out
}

fn render_anthropic(parts: &[Part], finish: Finish, usage: (u64, u64, u64), partial: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |ty: &str, mut data: Value| {
        data["type"] = json!(ty);
        out.push(sse_named(ty, data));
    };
    push(
        "message_start",
        json!({ "message": {
        "id": "msg_1", "type": "message", "role": "assistant", "content": [], "stop_reason": null, "stop_sequence": null,
        "usage": { "input_tokens": usage.0, "output_tokens": 0, "cache_read_input_tokens": usage.2, "cache_creation_input_tokens": 0 },
    } }),
    );
    for (idx, p) in parts.iter().enumerate() {
        match p {
            Part::Reasoning { text, signature } => {
                push(
                    "content_block_start",
                    json!({ "index": idx, "content_block": { "type": "thinking", "thinking": "", "signature": "" } }),
                );
                push(
                    "content_block_delta",
                    json!({ "index": idx, "delta": { "type": "thinking_delta", "thinking": text } }),
                );
                if let Some(sig) = signature {
                    push(
                        "content_block_delta",
                        json!({ "index": idx, "delta": { "type": "signature_delta", "signature": sig } }),
                    );
                }
                push("content_block_stop", json!({ "index": idx }));
            }
            Part::Text(text) => {
                push("content_block_start", json!({ "index": idx, "content_block": { "type": "text", "text": "" } }));
                push("content_block_delta", json!({ "index": idx, "delta": { "type": "text_delta", "text": text } }));
                push("content_block_stop", json!({ "index": idx }));
            }
            Part::ToolCall { id, name, args } | Part::SignedToolCall { id, name, args, .. } => {
                push(
                    "content_block_start",
                    json!({ "index": idx, "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } }),
                );
                push(
                    "content_block_delta",
                    json!({ "index": idx, "delta": { "type": "input_json_delta", "partial_json": args.to_string() } }),
                );
                push("content_block_stop", json!({ "index": idx }));
            }
            Part::PartialToolCall { id, name, raw_args } => {
                push(
                    "content_block_start",
                    json!({ "index": idx, "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } }),
                );
                push(
                    "content_block_delta",
                    json!({ "index": idx, "delta": { "type": "input_json_delta", "partial_json": raw_args } }),
                );
            }
            Part::Finish(_) | Part::Usage { .. } => {}
        }
    }
    if !partial {
        let stop = match finish {
            Finish::Stop => "end_turn",
            Finish::ToolCalls => "tool_use",
            Finish::Length => "max_tokens",
            Finish::ContentFilter => "refusal",
        };
        push(
            "message_delta",
            json!({
                "delta": { "stop_reason": stop, "stop_sequence": null },
                "usage": { "input_tokens": usage.0, "output_tokens": usage.1, "cache_read_input_tokens": usage.2, "cache_creation_input_tokens": 0 },
            }),
        );
        push("message_stop", json!({}));
    }
    out
}

fn render_compat(parts: &[Part], finish: Finish, usage: (u64, u64, u64), partial: bool) -> Vec<String> {
    let mut out = Vec::new();
    out.push(sse_data(json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk",
        "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }],
    })));
    let mut tool_idx = 0usize;
    for p in parts {
        match p {
            Part::Reasoning { text, .. } => out.push(sse_data(json!({
                "id": "chatcmpl-1", "object": "chat.completion.chunk",
                "choices": [{ "index": 0, "delta": { "reasoning_content": text }, "finish_reason": null }],
            }))),
            Part::Text(text) => out.push(sse_data(json!({
                "id": "chatcmpl-1", "object": "chat.completion.chunk",
                "choices": [{ "index": 0, "delta": { "content": text }, "finish_reason": null }],
            }))),
            Part::ToolCall { id, name, args } | Part::SignedToolCall { id, name, args, .. } => {
                out.push(sse_data(json!({
                    "id": "chatcmpl-1", "object": "chat.completion.chunk",
                    "choices": [{ "index": 0, "delta": { "tool_calls": [{
                        "index": tool_idx, "id": id, "type": "function",
                        "function": { "name": name, "arguments": args.to_string() },
                    }] }, "finish_reason": null }],
                })));
                tool_idx += 1;
            }
            Part::PartialToolCall { id, name, raw_args } => {
                out.push(sse_data(json!({
                    "id": "chatcmpl-1", "object": "chat.completion.chunk",
                    "choices": [{ "index": 0, "delta": { "tool_calls": [{
                        "index": tool_idx, "id": id, "type": "function",
                        "function": { "name": name, "arguments": raw_args },
                    }] }, "finish_reason": null }],
                })));
                tool_idx += 1;
            }
            Part::Finish(_) | Part::Usage { .. } => {}
        }
    }
    if !partial {
        let fr = match finish {
            Finish::Stop => "stop",
            Finish::ToolCalls => "tool_calls",
            Finish::Length => "length",
            Finish::ContentFilter => "content_filter",
        };
        out.push(sse_data(json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": fr }],
            "usage": { "prompt_tokens": usage.0 + usage.2, "completion_tokens": usage.1, "prompt_tokens_details": { "cached_tokens": usage.2 } },
        })));
        out.push("data: [DONE]\n\n".to_string());
    }
    out
}

fn render_gemini(parts: &[Part], finish: Finish, usage: (u64, u64, u64), partial: bool) -> Vec<String> {
    let mut out = Vec::new();
    for p in parts {
        let part = match p {
            Part::Reasoning { text, signature } => {
                let mut v = json!({ "thought": true, "text": text });
                if let Some(sig) = signature {
                    v["thoughtSignature"] = json!(sig);
                }
                v
            }
            Part::Text(text) => json!({ "text": text }),
            Part::ToolCall { id, name, args } => {
                let mut fc = json!({ "name": name, "args": args });
                if !id.is_empty() {
                    fc["id"] = json!(id);
                }
                json!({ "functionCall": fc })
            }
            Part::SignedToolCall { id, name, args, signature } => {
                let mut fc = json!({ "name": name, "args": args });
                if !id.is_empty() {
                    fc["id"] = json!(id);
                }
                json!({ "functionCall": fc, "thoughtSignature": signature })
            }
            Part::PartialToolCall { id, name, raw_args } => json!({
                "functionCall": { "id": id, "name": name, "partialArgs": raw_args, "willContinue": true },
            }),
            Part::Finish(_) | Part::Usage { .. } => continue,
        };
        out.push(sse_data(json!({
            "responseId": "resp_1",
            "candidates": [{ "content": { "role": "model", "parts": [part] } }],
        })));
    }
    if !partial {
        // Official Gemini FinishReason enum: STOP / MAX_TOKENS / SAFETY (never FUNCTION_CALL).
        let fr = match finish {
            Finish::Stop | Finish::ToolCalls => "STOP",
            Finish::Length => "MAX_TOKENS",
            Finish::ContentFilter => "SAFETY",
        };
        out.push(sse_data(json!({
            "responseId": "resp_1",
            "candidates": [{ "content": { "role": "model", "parts": [] }, "finishReason": fr }],
            "usageMetadata": {
                "promptTokenCount": usage.0 + usage.2,
                "candidatesTokenCount": usage.1,
                "cachedContentTokenCount": usage.2,
            },
        })));
    }
    out
}

pub fn render_step(wire: Wire, step: &Step) -> Resp {
    match step {
        Step::Reply(parts) => Resp::sse(render(wire, parts).concat()),
        Step::Empty => Resp::sse(render(wire, &[]).concat()),
        Step::Raw(r) => r.clone(),
        Step::HttpError { status, retry_after, message } => {
            let body = match wire {
                Wire::Anthropic => {
                    let ty = match *status {
                        429 => "rate_limit_error",
                        529 => "overloaded_error",
                        400 | 422 => "invalid_request_error",
                        401 | 403 => "authentication_error",
                        _ => "api_error",
                    };
                    json!({ "type": "error", "error": { "type": ty, "message": message } })
                }
                Wire::Gemini => {
                    let st = match *status {
                        429 => "RESOURCE_EXHAUSTED",
                        503 | 529 => "UNAVAILABLE",
                        400 | 422 => "INVALID_ARGUMENT",
                        401 | 403 => "UNAUTHENTICATED",
                        _ => "INTERNAL",
                    };
                    json!({ "error": { "code": status, "message": message, "status": st } })
                }
                Wire::Responses | Wire::Compat => {
                    let (ty, code) = match *status {
                        429 => ("rate_limit_error", "rate_limit_exceeded"),
                        401 | 403 => ("authentication_error", "invalid_api_key"),
                        400 | 422 => ("invalid_request_error", "invalid_request_error"),
                        _ => ("server_error", "server_error"),
                    };
                    json!({ "error": { "message": message, "type": ty, "code": code } })
                }
            };
            let mut r = Resp::status(*status, body.to_string());
            if let Some(ra) = retry_after {
                r = r.header("retry-after", ra);
            }
            r
        }
        Step::StreamError { message, code } => {
            let sse = match wire {
                Wire::Responses => {
                    sse_named(
                        "response.created",
                        json!({ "type": "response.created", "sequence_number": 0, "response": { "id": "resp_1", "status": "in_progress" } }),
                    ) + &sse_named(
                        "response.failed",
                        json!({ "type": "response.failed", "sequence_number": 1, "response": { "id": "resp_1", "status": "failed", "error": { "message": message, "code": code } } }),
                    )
                }
                Wire::Anthropic => {
                    sse_named(
                        "message_start",
                        json!({ "type": "message_start", "message": { "id": "msg_1", "type": "message", "role": "assistant", "content": [] } }),
                    ) + &sse_named(
                        "error",
                        json!({ "type": "error", "error": { "type": code.as_deref().unwrap_or("api_error"), "message": message } }),
                    )
                }
                Wire::Compat => sse_data(json!({ "error": { "message": message, "code": code } })) + "data: [DONE]\n\n",
                Wire::Gemini => sse_data(
                    json!({ "error": { "code": 500, "message": message, "status": code.as_deref().unwrap_or("INTERNAL") } }),
                ),
            };
            Resp::sse(sse)
        }
        Step::CutAfter { parts, events } => {
            let evs = render(wire, parts);
            let off: usize = evs.iter().take(*events).map(String::len).sum();
            Resp::sse(evs.concat()).cut(off)
        }
        Step::StallAfter { parts, events, ms } => {
            let evs = render(wire, parts);
            let off: usize = evs.iter().take(*events).map(String::len).sum();
            Resp::sse(evs.concat()).stall(off, *ms)
        }
    }
}
