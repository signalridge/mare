//! Unit tests for `common::scripted` (`ScriptedModel` renderers vs real recordings + invariants).

use std::collections::HashMap;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::common::scripted::{
    Finish, Part, ScriptedModel, Step, Wire, assert_wire, render, render_step, validate_request,
};
use crate::common::{Request, TestEnv, local};

fn assert_subsequence(haystack: &[&str], expected_seq: &[&str], label: &str) {
    let mut pos = 0;
    for &want in expected_seq {
        let rel = haystack[pos..]
            .iter()
            .position(|&got| got == want)
            .unwrap_or_else(|| panic!("{label}: missing {want} in order after index {pos}: {haystack:?}"));
        pos += rel + 1;
    }
}

fn fixture_path(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(rel)
}

fn parse_sse_json(raw: &str) -> Vec<(Option<String>, Value)> {
    let mut out = Vec::new();
    for frame in raw.split("\n\n").filter(|s| !s.trim().is_empty()) {
        let mut ev = None;
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(e) = line.strip_prefix("event:") {
                ev = Some(e.trim().to_string());
            } else if let Some(d) = line.strip_prefix("data:") {
                data.push_str(d.trim());
            }
        }
        if data == "[DONE]" || data.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&data) {
            out.push((ev, v));
        }
    }
    out
}

#[test]
fn renderer_responses_matches_replay_recording() {
    let raw =
        std::fs::read_to_string(fixture_path("cassettes/mare-record/opencode-go-deepseek-v4.1-flash.json")).unwrap();
    let rec: Value = serde_json::from_str(&raw).unwrap();
    let interactions = rec["interactions"].as_array().unwrap();
    let find_turn = |must_have: &[&str]| -> Vec<String> {
        interactions
            .iter()
            .filter_map(|item| item["response"]["body"].as_str())
            .map(|sse| parse_sse_json(sse).into_iter().filter_map(|(e, _)| e).collect::<Vec<_>>())
            .find(|types| must_have.iter().all(|need| types.iter().any(|t| t == need)))
            .unwrap_or_else(|| panic!("no recorded interaction contained all of {must_have:?}"))
    };
    let real_tool_owned =
        find_turn(&["response.reasoning_summary_text.delta", "response.function_call_arguments.delta"]);
    let real_tool_types: Vec<&str> = real_tool_owned.iter().map(String::as_str).collect();

    let rendered = render(
        Wire::Responses,
        &[
            Part::Reasoning { text: "先读取文件。".into(), signature: None },
            Part::ToolCall {
                id: "call_c2eb80704c4349c68fd5207b".into(),
                name: "read".into(),
                args: json!({ "path": "corpus.txt" }),
            },
        ],
    );
    let our_events = parse_sse_json(&rendered.concat());
    let our_types: Vec<&str> = our_events.iter().filter_map(|(e, _)| e.as_deref()).collect();

    let expected_tool_seq = &[
        "response.created",
        "response.output_item.added",
        "response.reasoning_summary_text.delta",
        "response.output_item.added",
        "response.function_call_arguments.delta",
        "response.output_item.done",
        "response.completed",
    ];
    assert_subsequence(&real_tool_types, expected_tool_seq, "real responses tool turn");
    assert_subsequence(&our_types, expected_tool_seq, "rendered responses tool turn");

    let real_text_owned = find_turn(&["response.reasoning_summary_text.delta", "response.output_text.delta"]);
    let real_text_types: Vec<&str> = real_text_owned.iter().map(String::as_str).collect();
    let rendered_text = render(
        Wire::Responses,
        &[Part::Reasoning { text: "整理回答。".into(), signature: None }, Part::Text("我来读取这个文件。".into())],
    );
    let our_text_events = parse_sse_json(&rendered_text.concat());
    let our_text_types: Vec<&str> = our_text_events.iter().filter_map(|(e, _)| e.as_deref()).collect();
    let expected_text_seq = &[
        "response.created",
        "response.output_item.added",
        "response.reasoning_summary_text.delta",
        "response.output_item.added",
        "response.output_text.delta",
        "response.output_item.done",
        "response.completed",
    ];
    assert_subsequence(&real_text_types, expected_text_seq, "real responses text turn");
    assert_subsequence(&our_text_types, expected_text_seq, "rendered responses text turn");
    let fc_done = our_events
        .iter()
        .find(|(e, v)| e.as_deref() == Some("response.output_item.done") && v["item"]["type"] == "function_call")
        .unwrap()
        .1
        .clone();
    assert_eq!(fc_done["item"]["call_id"], "call_c2eb80704c4349c68fd5207b");
    assert_eq!(fc_done["item"]["name"], "read");
    assert_eq!(
        serde_json::from_str::<Value>(fc_done["item"]["arguments"].as_str().unwrap()).unwrap(),
        json!({ "path": "corpus.txt" })
    );
}

#[test]
fn renderer_anthropic_matches_replay_recording() {
    let raw = std::fs::read_to_string(fixture_path("cassettes/mare-record/opencode-go-claude-haiku-5-5.json")).unwrap();
    let rec: Value = serde_json::from_str(&raw).unwrap();
    let real_sse = rec["interactions"][1]["response"]["body"].as_str().unwrap();
    let real_events = parse_sse_json(real_sse);
    let real_types: Vec<&str> = real_events.iter().filter_map(|(e, _)| e.as_deref()).collect();

    let rendered = render(
        Wire::Anthropic,
        &[
            Part::Reasoning { text: "check file".into(), signature: Some("sig-real-1".into()) },
            Part::ToolCall {
                id: "toolu_01D1dYBjChYqrE7EKia8A8Hh".into(),
                name: "bash".into(),
                args: json!({ "command": "wc -l" }),
            },
        ],
    );
    let our_events = parse_sse_json(&rendered.concat());
    let our_types: Vec<&str> = our_events.iter().filter_map(|(e, _)| e.as_deref()).collect();

    let expected_seq = &[
        "message_start",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "message_delta",
        "message_stop",
    ];
    assert_subsequence(&real_types, expected_seq, "real anthropic");
    assert_subsequence(&our_types, expected_seq, "rendered anthropic");
    let sig_delta = our_events.iter().find(|(_, v)| v["delta"]["type"] == "signature_delta").unwrap();
    assert_eq!(sig_delta.1["delta"]["signature"], "sig-real-1");
    let msg_delta = our_events.iter().find(|(e, _)| e.as_deref() == Some("message_delta")).unwrap();
    assert_eq!(msg_delta.1["delta"]["stop_reason"], "tool_use");
}

#[test]
fn renderer_compat_matches_usage_hoist_fixture() {
    let real_sse = std::fs::read_to_string(fixture_path("providers/openai-compat/usage-hoist.sse")).unwrap();
    assert!(real_sse.contains("data: [DONE]"));
    let real_events = parse_sse_json(&real_sse);
    assert_eq!(real_events[0].1["choices"][0]["finish_reason"], "stop");

    let rendered = render(Wire::Compat, &[Part::Text("ok".into()), Part::Usage { input: 5, output: 1, cached: 0 }]);
    let full = rendered.concat();
    assert!(full.ends_with("data: [DONE]\n\n"));
    let our_events = parse_sse_json(&full);
    assert!(our_events.iter().any(|(_, v)| v["choices"][0]["delta"]["content"] == "ok"));
    let last = &our_events.last().unwrap().1;
    assert_eq!(last["choices"][0]["finish_reason"], "stop");
    assert_eq!(last["usage"]["prompt_tokens"], 5);
    assert_eq!(last["usage"]["completion_tokens"], 1);
}

#[test]
fn renderer_gemini_matches_fixture_and_official_enums() {
    let ts = std::fs::read_to_string(fixture_path("providers/google/function-call.ts")).unwrap();
    assert!(ts.contains("finishReason: 'STOP'"), "gemini fixture uses official STOP enum");

    let rendered = render(
        Wire::Gemini,
        &[
            Part::Reasoning { text: "reason".into(), signature: Some("sig-think".into()) },
            Part::SignedToolCall {
                id: "native-call".into(),
                name: "weather".into(),
                args: json!({ "city": "sf" }),
                signature: "sig-tool".into(),
            },
            Part::Usage { input: 8, output: 3, cached: 2 },
        ],
    );
    let our_events = parse_sse_json(&rendered.concat());
    assert_eq!(our_events[0].1["candidates"][0]["content"]["parts"][0]["thought"], true);
    assert_eq!(our_events[0].1["candidates"][0]["content"]["parts"][0]["thoughtSignature"], "sig-think");
    assert_eq!(our_events[1].1["candidates"][0]["content"]["parts"][0]["functionCall"]["id"], "native-call");
    assert_eq!(our_events[1].1["candidates"][0]["content"]["parts"][0]["functionCall"]["name"], "weather");
    assert_eq!(our_events[1].1["candidates"][0]["content"]["parts"][0]["thoughtSignature"], "sig-tool");

    // Default ToolCall on Gemini omits functionCall.id (when empty) and thoughtSignature.
    let unsigned = parse_sse_json(
        &render(Wire::Gemini, &[Part::ToolCall { id: String::new(), name: "read".into(), args: json!({}) }]).concat(),
    );
    let fc_part = &unsigned[0].1["candidates"][0]["content"]["parts"][0];
    assert!(fc_part["functionCall"].get("id").is_none(), "expected omitted id: {fc_part}");
    assert!(fc_part.get("thoughtSignature").is_none(), "expected omitted thoughtSignature: {fc_part}");
    let fin = &our_events[2].1;
    assert_eq!(fin["candidates"][0]["finishReason"], "STOP");
    assert_eq!(fin["usageMetadata"]["promptTokenCount"], 10);
    assert_eq!(fin["usageMetadata"]["cachedContentTokenCount"], 2);

    let len_ev =
        parse_sse_json(&render(Wire::Gemini, &[Part::Text("x".into()), Part::Finish(Finish::Length)]).concat());
    assert_eq!(len_ev.last().unwrap().1["candidates"][0]["finishReason"], "MAX_TOKENS");
    let saf_ev =
        parse_sse_json(&render(Wire::Gemini, &[Part::Text("x".into()), Part::Finish(Finish::ContentFilter)]).concat());
    assert_eq!(saf_ev.last().unwrap().1["candidates"][0]["finishReason"], "SAFETY");
}

fn req(path: &str, body: Value) -> Request {
    Request { method: "POST".into(), path: path.to_string(), body: body.to_string(), headers: vec![] }
}

#[test]
fn invariants_catch_every_violation_class() {
    let call = |id: &str| json!({ "type": "function_call", "call_id": id, "name": "read", "arguments": "{}" });
    let out = |id: &str| json!({ "type": "function_call_output", "call_id": id, "output": "x" });
    let sigs = HashMap::new();

    // 1. Orphan tool_result
    let errs = validate_request(Wire::Responses, &req("/v1/responses", json!({ "input": [out("c9")] })), &[], &sigs);
    assert!(errs.iter().any(|e| e.contains("orphan")), "{errs:?}");

    // 2. Unanswered tool_call vs allow_pending
    let errs = validate_request(Wire::Responses, &req("/v1/responses", json!({ "input": [call("c1")] })), &[], &sigs);
    assert!(errs.iter().any(|e| e.contains("never answered")), "{errs:?}");
    let ok = validate_request(
        Wire::Responses,
        &req("/v1/responses", json!({ "input": [call("c1")] })),
        &["c1".to_string()],
        &sigs,
    );
    assert!(ok.is_empty(), "{ok:?}");

    // 3. Empty id and duplicate id
    let errs =
        validate_request(Wire::Responses, &req("/v1/responses", json!({ "input": [call(""), out("")] })), &[], &sigs);
    assert!(errs.iter().any(|e| e.contains("without id")), "{errs:?}");
    let errs = validate_request(
        Wire::Responses,
        &req("/v1/responses", json!({ "input": [call("c1"), out("c1"), call("c1"), out("c1")] })),
        &[],
        &sigs,
    );
    assert!(errs.iter().any(|e| e.contains("used twice")), "{errs:?}");

    // 4. Anthropic thinking signature roundtrip
    let mut expected_sigs = HashMap::new();
    expected_sigs.insert("deep thought".to_string(), "sig_valid_123".to_string());
    let bad_sig = req(
        "/v1/messages",
        json!({
            "max_tokens": 100,
            "messages": [{ "role": "assistant", "content": [{ "type": "thinking", "thinking": "deep thought", "signature": "sig_tampered" }] }]
        }),
    );
    let errs = validate_request(Wire::Anthropic, &bad_sig, &[], &expected_sigs);
    assert!(errs.iter().any(|e| e.contains("signature mismatch")), "{errs:?}");

    // 5. Responses function_call_output.call_id mismatch
    let mismatched = req("/v1/responses", json!({ "input": [call("c1"), out("c_other")] }));
    let errs = validate_request(Wire::Responses, &mismatched, &[], &sigs);
    assert!(errs.iter().any(|e| e.contains("c_other")), "{errs:?}");

    // 6. Wire shape checks (assert_wire)
    assert!(
        assert_wire(Wire::Anthropic, &req("/v1/messages", json!({ "messages": [] }))).is_err(),
        "missing max_tokens"
    );
    assert!(assert_wire(Wire::Compat, &req("/v1/responses", json!({ "messages": [] }))).is_err(), "wrong path");
}

async fn post_json(base: &str, path: &str, body: Value) {
    let addr = base.strip_prefix("http://").unwrap();
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    let payload = body.to_string();
    let raw = format!(
        "POST {path} HTTP/1.0\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
        payload.len()
    );
    sock.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = sock.read_to_end(&mut buf).await;
}

#[tokio::test]
async fn scripted_model_finish_reports_capture_time_violations_and_count_mismatch() {
    let mut m = ScriptedModel::start(Wire::Responses, vec![Step::Reply(vec![Part::Text("ok".into())])]).await;
    // Send an invalid request (orphan tool result) -> finish() must report BOTH the invariant violation and count if mismatched.
    post_json(
        m.base(),
        "/v1/responses",
        json!({ "input": [{ "type": "function_call_output", "call_id": "orphan_1", "output": "x" }] }),
    )
    .await;
    let err = m.check_finish().unwrap_err();
    assert!(err.contains("orphan_1"), "missing invariant error: {err}");

    let mut m2 = ScriptedModel::start(Wire::Compat, vec![Step::Reply(vec![Part::Text("ok".into())])]).await;
    let err2 = m2.check_finish().unwrap_err();
    assert!(err2.contains("expected 1 request(s), got 0"), "missing count mismatch: {err2}");
}

#[tokio::test]
async fn scripted_model_drop_without_finish_panics() {
    let res = tokio::spawn(async {
        let _m = ScriptedModel::start(Wire::Compat, vec![Step::Empty]).await;
    })
    .await;
    assert!(res.unwrap_err().is_panic(), "Drop without finish() must panic");
}

#[tokio::test]
async fn http_error_renders_wire_native_bodies_and_classifies_errors() {
    let body_of = |wire: Wire, status: u16| -> Value {
        let r = render_step(wire, &Step::HttpError { status, retry_after: None, message: "boom".into() });
        serde_json::from_str(&r.body).unwrap()
    };
    assert_eq!(body_of(Wire::Anthropic, 429)["type"], "error");
    assert_eq!(body_of(Wire::Anthropic, 429)["error"]["type"], "rate_limit_error");
    assert_eq!(body_of(Wire::Anthropic, 529)["error"]["type"], "overloaded_error");
    assert_eq!(body_of(Wire::Gemini, 429)["error"]["status"], "RESOURCE_EXHAUSTED");
    assert_eq!(body_of(Wire::Gemini, 429)["error"]["code"], 429);
    assert_eq!(body_of(Wire::Responses, 429)["error"]["code"], "rate_limit_exceeded");
    assert_eq!(body_of(Wire::Compat, 401)["error"]["type"], "authentication_error");

    for wire in [Wire::Anthropic, Wire::Gemini] {
        local(async {
            let mut env = TestEnv::new().await;
            let mock = ScriptedModel::start(
                wire,
                vec![
                    Step::HttpError { status: 429, retry_after: Some("0".into()), message: "rate limited".into() },
                    Step::Reply(vec![Part::Text("recovered".into())]),
                ],
            )
            .await;
            let h = mock.host(&mut env, false, &[]).await;
            let res = h.prompt("hi", json!({})).await.unwrap();
            assert_eq!(res["text"], "recovered", "{wire}: {res}");
            h.shutdown().await;
            mock.finish();
        })
        .await;
    }
}

fn parse_ndjson(raw: &str) -> Vec<Value> {
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap_or_else(|e| panic!("invalid ndjson line: {e}: {l}")))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedUsage {
    input: u64,
    output: u64,
    cached: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedStream {
    event_types: Vec<String>,
    tool_calls: Vec<(String, Value)>,
    finish_reason: Option<String>,
    signatures: Vec<String>,
    usage: Option<NormalizedUsage>,
}

fn push_dedup(out: &mut Vec<String>, ty: &str) {
    if out.last().map(String::as_str) != Some(ty) {
        out.push(ty.to_string());
    }
}

fn normalize_responses(events: &[(Option<String>, Value)]) -> NormalizedStream {
    let mut event_types = Vec::new();
    let mut tool_calls = Vec::new();
    let mut finish_reason = None;
    let mut signatures = Vec::new();
    let mut usage = None;

    for (ev, v) in events {
        let Some(ty) = ev.as_deref() else { continue };
        if ty == "response.in_progress" {
            continue;
        }
        push_dedup(&mut event_types, ty);
        match ty {
            "response.output_item.done" => {
                let item = &v["item"];
                match item["type"].as_str() {
                    Some("function_call") => {
                        let name = item["name"].as_str().unwrap_or_default().to_string();
                        let args: Value = item["arguments"]
                            .as_str()
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or(Value::Null);
                        tool_calls.push((name, args));
                    }
                    Some("reasoning") => {
                        if let Some(sig) = item["encrypted_content"].as_str() {
                            signatures.push(sig.to_string());
                        }
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                let resp = &v["response"];
                finish_reason = resp["incomplete_details"]["reason"]
                    .as_str()
                    .or_else(|| resp["status"].as_str())
                    .map(str::to_string);
                if let Some(u) = resp.get("usage").filter(|u| !u.is_null()) {
                    usage = Some(NormalizedUsage {
                        input: u["input_tokens"].as_u64().unwrap_or(0),
                        output: u["output_tokens"].as_u64().unwrap_or(0),
                        cached: u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
                    });
                }
            }
            _ => {}
        }
    }

    NormalizedStream { event_types, tool_calls, finish_reason, signatures, usage }
}

fn normalize_anthropic(events: &[Value]) -> NormalizedStream {
    let mut event_types = Vec::new();
    let mut tool_calls = Vec::new();
    let mut finish_reason = None;
    let mut signatures = Vec::new();
    let mut usage = None;
    let mut active_tools: HashMap<u64, (String, String)> = HashMap::new();

    for v in events {
        let Some(ty) = v["type"].as_str() else { continue };
        if ty == "ping" {
            continue;
        }
        push_dedup(&mut event_types, ty);
        match ty {
            "content_block_start" => {
                let idx = v["index"].as_u64().unwrap_or(0);
                if v["content_block"]["type"] == "tool_use" {
                    let name = v["content_block"]["name"].as_str().unwrap_or_default().to_string();
                    active_tools.insert(idx, (name, String::new()));
                }
            }
            "content_block_delta" => {
                let idx = v["index"].as_u64().unwrap_or(0);
                match v["delta"]["type"].as_str() {
                    Some("input_json_delta") => {
                        if let Some((_, buf)) = active_tools.get_mut(&idx)
                            && let Some(part) = v["delta"]["partial_json"].as_str()
                        {
                            buf.push_str(part);
                        }
                    }
                    Some("signature_delta") => {
                        if let Some(sig) = v["delta"]["signature"].as_str() {
                            signatures.push(sig.to_string());
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let idx = v["index"].as_u64().unwrap_or(0);
                if let Some((name, raw)) = active_tools.remove(&idx) {
                    let args = serde_json::from_str(&raw).unwrap_or(Value::Null);
                    tool_calls.push((name, args));
                }
            }
            "message_delta" => {
                finish_reason = v["delta"]["stop_reason"].as_str().map(str::to_string);
                if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                    usage = Some(NormalizedUsage {
                        input: u["input_tokens"].as_u64().unwrap_or(0),
                        output: u["output_tokens"].as_u64().unwrap_or(0),
                        cached: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                    });
                }
            }
            _ => {}
        }
    }

    NormalizedStream { event_types, tool_calls, finish_reason, signatures, usage }
}

fn normalize_compat(events: &[Value]) -> NormalizedStream {
    let mut event_types = Vec::new();
    let mut tool_calls_map: Vec<(String, String)> = Vec::new();
    let mut finish_reason = None;
    let mut usage = None;

    for v in events {
        let choice = &v["choices"][0];
        let delta = &choice["delta"];
        if delta.get("role").is_some_and(|r| !r.is_null()) {
            push_dedup(&mut event_types, "role");
        }
        if delta.get("reasoning_content").is_some_and(|r| !r.is_null()) {
            push_dedup(&mut event_types, "reasoning");
        }
        if delta.get("content").is_some_and(|c| !c.is_null()) {
            push_dedup(&mut event_types, "content");
        }
        if let Some(tcs) = delta["tool_calls"].as_array() {
            push_dedup(&mut event_types, "tool_calls");
            for tc in tcs {
                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                while tool_calls_map.len() <= idx {
                    tool_calls_map.push((String::new(), String::new()));
                }
                if let Some(name) = tc["function"]["name"].as_str() {
                    tool_calls_map[idx].0.push_str(name);
                }
                if let Some(args) = tc["function"]["arguments"].as_str() {
                    tool_calls_map[idx].1.push_str(args);
                }
            }
        }
        if let Some(fr) = choice["finish_reason"].as_str() {
            push_dedup(&mut event_types, "finish");
            finish_reason = Some(fr.to_string());
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            usage = Some(NormalizedUsage {
                input: u["prompt_tokens"].as_u64().unwrap_or(0),
                output: u["completion_tokens"].as_u64().unwrap_or(0),
                cached: u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
            });
        }
    }

    let tool_calls = tool_calls_map
        .into_iter()
        .map(|(name, raw)| (name, serde_json::from_str(&raw).unwrap_or(Value::Null)))
        .collect();
    NormalizedStream { event_types, tool_calls, finish_reason, signatures: Vec::new(), usage }
}

fn set_json_path(root: &mut Value, path: &str, val: Value) {
    let rest = path.strip_prefix("$.").unwrap_or(path);
    let mut cur = root;
    let mut segments = rest.split('.').peekable();
    while let Some(seg) = segments.next() {
        let is_last = segments.peek().is_none();
        if let Some((field, idx_str)) = seg.split_once('[') {
            let idx: usize = idx_str.trim_end_matches(']').parse().expect("valid array index");
            if !cur.is_object() {
                *cur = json!({});
            }
            let arr = cur.as_object_mut().unwrap().entry(field).or_insert_with(|| json!([]));
            let vec = arr.as_array_mut().unwrap();
            while vec.len() <= idx {
                vec.push(json!({}));
            }
            if is_last {
                vec[idx] = val;
                return;
            }
            cur = &mut vec[idx];
        } else {
            if !cur.is_object() {
                *cur = json!({});
            }
            let obj = cur.as_object_mut().unwrap();
            if is_last {
                match (obj.get_mut(seg), val) {
                    (Some(Value::String(existing)), Value::String(s)) => existing.push_str(&s),
                    (_, v) => {
                        obj.insert(seg.to_string(), v);
                    }
                }
                return;
            }
            cur = obj.entry(seg).or_insert_with(|| json!({}));
        }
    }
}

fn normalize_gemini(events: &[Value]) -> NormalizedStream {
    let mut event_types = Vec::new();
    let mut tool_calls: Vec<(String, Value)> = Vec::new();
    let mut finish_reason = None;
    let mut signatures = Vec::new();
    let mut usage = None;

    for v in events {
        let cand = &v["candidates"][0];
        if let Some(parts) = cand["content"]["parts"].as_array() {
            for part in parts {
                if let Some(sig) = part["thoughtSignature"].as_str() {
                    signatures.push(sig.to_string());
                }
                if part.get("thought") == Some(&Value::Bool(true)) {
                    push_dedup(&mut event_types, "thought");
                } else if let Some(fc) = part.get("functionCall") {
                    push_dedup(&mut event_types, "function_call");
                    if let Some(name) = fc["name"].as_str() {
                        let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                        tool_calls.push((name.to_string(), args));
                    }
                    if let Some(partials) = fc["partialArgs"].as_array()
                        && let Some((_, args)) = tool_calls.last_mut()
                    {
                        for pa in partials {
                            let jp = pa["jsonPath"].as_str().unwrap_or_default();
                            let val = if let Some(s) = pa["stringValue"].as_str() {
                                json!(s)
                            } else if let Some(n) = pa.get("numberValue") {
                                n.clone()
                            } else if let Some(b) = pa.get("boolValue") {
                                b.clone()
                            } else {
                                continue;
                            };
                            set_json_path(args, jp, val);
                        }
                    }
                } else if let Some(txt) = part["text"].as_str()
                    && !txt.is_empty()
                {
                    push_dedup(&mut event_types, "text");
                }
            }
        }
        if let Some(fr) = cand["finishReason"].as_str() {
            push_dedup(&mut event_types, "finish");
            finish_reason = Some(fr.to_string());
        }
        if let Some(u) = v.get("usageMetadata").filter(|u| u.get("promptTokenCount").is_some()) {
            usage = Some(NormalizedUsage {
                input: u["promptTokenCount"].as_u64().unwrap_or(0),
                output: u["candidatesTokenCount"].as_u64().unwrap_or(0),
                cached: u["cachedContentTokenCount"].as_u64().unwrap_or(0),
            });
        }
    }

    NormalizedStream { event_types, tool_calls, finish_reason, signatures, usage }
}

fn compare_normalized(
    real: &NormalizedStream,
    ours: &NormalizedStream,
    exact_events: bool,
    label: &str,
) -> Result<(), String> {
    if exact_events && real.event_types != ours.event_types {
        return Err(format!("{label}: event order mismatch: real={:?} ours={:?}", real.event_types, ours.event_types));
    }
    if real.tool_calls != ours.tool_calls {
        return Err(format!("{label}: tool_calls mismatch: real={:?} ours={:?}", real.tool_calls, ours.tool_calls));
    }
    if real.finish_reason != ours.finish_reason {
        return Err(format!(
            "{label}: finish_reason mismatch: real={:?} ours={:?}",
            real.finish_reason, ours.finish_reason
        ));
    }
    if real.signatures != ours.signatures {
        return Err(format!("{label}: signatures mismatch: real={:?} ours={:?}", real.signatures, ours.signatures));
    }
    if real.usage != ours.usage {
        return Err(format!("{label}: usage mismatch: real={:?} ours={:?}", real.usage, ours.usage));
    }
    Ok(())
}

#[test]
fn provider_fixtures_match_rendered_structure_for_all_four_wires() {
    // 1. Responses (reasoning + text + function_call + usage)
    let real_resp = parse_sse_json(
        &std::fs::read_to_string(fixture_path("providers/openai-responses/reasoning-then-call.sse")).unwrap(),
    );
    let real_resp_norm = normalize_responses(&real_resp);
    let real_resp_reasoning = real_resp
        .iter()
        .find_map(|(e, v)| {
            (e.as_deref() == Some("response.reasoning_summary_text.done"))
                .then(|| v["text"].as_str().unwrap().to_string())
        })
        .unwrap();
    let real_resp_text = real_resp
        .iter()
        .find_map(|(e, v)| {
            (e.as_deref() == Some("response.output_text.done")).then(|| v["text"].as_str().unwrap().to_string())
        })
        .unwrap();
    let real_resp_call_id = real_resp
        .iter()
        .find_map(|(e, v)| {
            (e.as_deref() == Some("response.output_item.done") && v["item"]["type"] == "function_call")
                .then(|| v["item"]["call_id"].as_str().unwrap().to_string())
        })
        .unwrap();
    let resp_usage = real_resp_norm.usage.as_ref().unwrap();
    let our_resp = parse_sse_json(
        &render(
            Wire::Responses,
            &[
                Part::Reasoning { text: real_resp_reasoning, signature: real_resp_norm.signatures.first().cloned() },
                Part::Text(real_resp_text),
                Part::ToolCall {
                    id: real_resp_call_id,
                    name: real_resp_norm.tool_calls[0].0.clone(),
                    args: real_resp_norm.tool_calls[0].1.clone(),
                },
                Part::Usage { input: resp_usage.input, output: resp_usage.output, cached: resp_usage.cached },
            ],
        )
        .concat(),
    );
    let our_resp_norm = normalize_responses(&our_resp);
    let resp_seq = &[
        "response.created",
        "response.output_item.added",
        "response.reasoning_summary_part.added",
        "response.reasoning_summary_text.delta",
        "response.output_item.added",
        "response.output_text.delta",
        "response.output_item.added",
        "response.function_call_arguments.delta",
        "response.completed",
    ];
    let real_resp_refs: Vec<&str> = real_resp_norm.event_types.iter().map(String::as_str).collect();
    let our_resp_refs: Vec<&str> = our_resp_norm.event_types.iter().map(String::as_str).collect();
    assert_subsequence(&real_resp_refs, resp_seq, "fixture responses");
    assert_subsequence(&our_resp_refs, resp_seq, "rendered responses");
    compare_normalized(&real_resp_norm, &our_resp_norm, false, "responses").unwrap();

    // 2. Anthropic (text + finish + usage)
    let real_anth = parse_ndjson(
        &std::fs::read_to_string(fixture_path("providers/anthropic/message-delta-input-tokens.chunks.txt")).unwrap(),
    );
    let real_anth_norm = normalize_anthropic(&real_anth);
    let real_anth_text: String = real_anth
        .iter()
        .filter(|v| v["type"] == "content_block_delta" && v["delta"]["type"] == "text_delta")
        .filter_map(|v| v["delta"]["text"].as_str())
        .collect();
    let anth_usage = real_anth_norm.usage.as_ref().unwrap();
    let our_anth = parse_sse_json(
        &render(
            Wire::Anthropic,
            &[
                Part::Text(real_anth_text),
                Part::Finish(Finish::Stop),
                Part::Usage { input: anth_usage.input, output: anth_usage.output, cached: anth_usage.cached },
            ],
        )
        .concat(),
    );
    let our_anth_vals: Vec<Value> = our_anth.into_iter().map(|(_, v)| v).collect();
    let our_anth_norm = normalize_anthropic(&our_anth_vals);
    compare_normalized(&real_anth_norm, &our_anth_norm, true, "anthropic").unwrap();

    // 3. Compat (role delta + finish + usage + [DONE])
    let real_compat_raw =
        std::fs::read_to_string(fixture_path("providers/openai-compat/gateway-metadata-stream.sse")).unwrap();
    assert!(real_compat_raw.contains("data: [DONE]"));
    let real_compat: Vec<Value> = real_compat_raw
        .lines()
        .filter_map(|l| l.strip_prefix("data:").map(str::trim))
        .filter(|d| !d.is_empty() && *d != "[DONE]")
        .map(|d| serde_json::from_str::<Value>(d).unwrap())
        .collect();
    let real_compat_norm = normalize_compat(&real_compat);
    let compat_usage = real_compat_norm.usage.as_ref().unwrap();
    let compat_finish = match real_compat_norm.finish_reason.as_deref() {
        Some("length") => Finish::Length,
        Some("content_filter") => Finish::ContentFilter,
        Some("tool_calls") => Finish::ToolCalls,
        _ => Finish::Stop,
    };
    let our_compat_raw = render(
        Wire::Compat,
        &[
            Part::Finish(compat_finish),
            Part::Usage { input: compat_usage.input, output: compat_usage.output, cached: compat_usage.cached },
        ],
    )
    .concat();
    assert!(our_compat_raw.ends_with("data: [DONE]\n\n"));
    let our_compat: Vec<Value> = parse_sse_json(&our_compat_raw).into_iter().map(|(_, v)| v).collect();
    let our_compat_norm = normalize_compat(&our_compat);
    compare_normalized(&real_compat_norm, &our_compat_norm, true, "compat").unwrap();

    // 4. Gemini (partialArgs array arguments without terminal functionCall)
    let real_gem = parse_ndjson(
        &std::fs::read_to_string(fixture_path("providers/google/stream-tool-call-array-args.chunks.txt")).unwrap(),
    );
    let real_gem_norm = normalize_gemini(&real_gem);
    let gem_usage = real_gem_norm.usage.as_ref().unwrap();
    let our_gem = parse_sse_json(
        &render(
            Wire::Gemini,
            &[
                Part::SignedToolCall {
                    id: String::new(),
                    name: real_gem_norm.tool_calls[0].0.clone(),
                    args: real_gem_norm.tool_calls[0].1.clone(),
                    signature: real_gem_norm.signatures[0].clone(),
                },
                Part::Finish(Finish::Stop),
                Part::Usage { input: gem_usage.input, output: gem_usage.output, cached: gem_usage.cached },
            ],
        )
        .concat(),
    );
    let our_gem_vals: Vec<Value> = our_gem.into_iter().map(|(_, v)| v).collect();
    let our_gem_norm = normalize_gemini(&our_gem_vals);
    compare_normalized(&real_gem_norm, &our_gem_norm, true, "gemini").unwrap();
}

#[test]
fn normalized_comparison_rejects_field_and_order_mismatches() {
    let base = NormalizedStream {
        event_types: vec!["a".into(), "b".into()],
        tool_calls: vec![("read".into(), json!({ "path": "a.txt" }))],
        finish_reason: Some("stop".into()),
        signatures: vec!["sig-1".into()],
        usage: Some(NormalizedUsage { input: 10, output: 5, cached: 2 }),
    };
    assert!(compare_normalized(&base, &base, true, "same").is_ok());

    let mut bad_order = base.clone();
    bad_order.event_types.reverse();
    assert!(compare_normalized(&base, &bad_order, true, "order").unwrap_err().contains("event order"));

    let mut bad_args = base.clone();
    bad_args.tool_calls[0].1 = json!({ "path": "b.txt" });
    assert!(compare_normalized(&base, &bad_args, true, "args").unwrap_err().contains("tool_calls"));

    let mut bad_finish = base.clone();
    bad_finish.finish_reason = Some("length".into());
    assert!(compare_normalized(&base, &bad_finish, true, "finish").unwrap_err().contains("finish_reason"));

    let mut bad_sig = base.clone();
    bad_sig.signatures[0] = "sig-tampered".into();
    assert!(compare_normalized(&base, &bad_sig, true, "sig").unwrap_err().contains("signatures"));

    let mut bad_usage = base.clone();
    bad_usage.usage.as_mut().unwrap().cached = 99;
    assert!(compare_normalized(&base, &bad_usage, true, "usage").unwrap_err().contains("usage"));
}
