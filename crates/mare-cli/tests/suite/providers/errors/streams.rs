use super::*;

#[tokio::test]
async fn auth_error_is_surfaced_without_retry() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/compat", vec![Resp::status(401, r#"{"error":{"message":"bad key"}}"#)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let f = h.lua().load(STREAM).into_function().unwrap();
        let err = f.call_async::<mlua::Value>("compat/reasoner-model").await;
        let text = match err {
            Err(e) => e.to_string(),
            Ok(v) => mare_shared::lua::lua_to_json(h.lua(), v).unwrap().to_string(),
        };
        assert!(text.contains("401") || text.contains("bad key"), "{text}");
        assert_eq!(server.count("/compat"), 1, "4xx must not be retried");
        h.shutdown().await;
    })
    .await
}

/// "Too many images" is its own kind in every provider (the agent offloads images
/// and sends again) — not a rate limit ("too many"), not a context overflow, never retried as is.
#[tokio::test]
async fn too_many_images_is_an_image_limit_in_every_provider() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let anthropic = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.0.content: Too many images in request. Maximum is 100."}}"#;
        let openai = r#"{"error":{"message":"Too many images in request. Max is 500.","type":"invalid_request_error","code":null}}"#;
        let google = r#"{"error":{"code":400,"message":"The number of images in the request exceeds the limit.","status":"INVALID_ARGUMENT"}}"#;
        let compat = r#"{"error":{"message":"image exceeds the maximum allowed size"}}"#;
        server.route("/anthropic", vec![Resp::status(400, anthropic)]);
        server.route("/openai", vec![Resp::status(400, openai)]);
        server.route("/google", vec![Resp::status(400, google)]);
        server.route("/compat", vec![Resp::status(400, compat)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        for (model, route) in [
            ("anthropic/claude-sonnet-5-5", "/anthropic"),
            ("openai/gpt-6.1-terra", "/openai"),
            ("google/gemini-2.5-pro", "/google"),
            ("compat/reasoner-model", "/compat"),
        ] {
            let msg = stream(&h, model).await;
            assert_eq!(msg["error_kind"], "image_limit", "{model}: {msg}");
            assert_eq!(server.count(route), 1, "{model}: not retried as is");
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_too_large_request_body_is_not_a_context_overflow() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/compat", vec![Resp::status(413, r#"{"error":{"message":"Request Entity Too Large"}}"#)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = stream(&h, "compat/reasoner-model").await;
        assert_eq!(msg["error_kind"], "payload_too_large", "{msg}");
        assert!(msg["error"].as_str().unwrap().contains("remove large images"), "{msg}");
        assert_eq!(server.count("/compat"), 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn anthropic_malformed_streams_fail_without_retry() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let no_id = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"name\":\"read\"}}\n\n";
        let garbage = "event: message_start\ndata: {not json\n\n";
        server.route("/anthropic", vec![Resp::sse(no_id), Resp::sse(garbage)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        for n in 1..=2 {
            let msg = stream(&h, "anthropic/claude-sonnet-5-5").await;
            assert_eq!(msg["error_kind"], "malformed_response", "{msg}");
            assert_eq!(server.count("/anthropic"), n, "not retried");
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_request_that_keeps_crashing_mare_is_not_sent_again() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        // three runs died while this request was in flight (each left its mark): the guard is in
        // core, so it holds whatever plugin sends the request
        let msg: mlua::Value = h
            .lua()
            .load(r#"
local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
local req = { model = m, session_id = "s1", messages = { { role = "user", content = { { type = "text", text = "boom" } } } } }
local fp = LLM:fingerprint(req)
for _ = 1, 3 do mare.crash.begin(fp) end
return LLM:stream(req, function() end)
"#)
            .eval_async()
            .await
            .unwrap();
        let msg = mare_shared::lua::lua_to_json(h.lua(), msg).unwrap();
        assert_eq!(msg["error_kind"], "crash_limit", "{msg}");
        assert!(msg["error"].as_str().unwrap().contains("crashed mare 3 times"), "{msg}");
        assert_eq!(server.count("/anthropic"), 0, "not sent");
        // asking again: still refused, and the refusal is not counted as another crash
        let again: mlua::Value = h
            .lua()
            .load(r#"
local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
return LLM:stream({ model = m, session_id = "s1", messages = { { role = "user", content = { { type = "text", text = "boom" } } } } }, function() end)
"#)
            .eval_async()
            .await
            .unwrap();
        let again = mare_shared::lua::lua_to_json(h.lua(), again).unwrap();
        assert_eq!(again["error_kind"], "crash_limit", "{again}");
        assert!(again["error"].as_str().unwrap().contains("crashed mare 3 times"), "{again}");
        assert_eq!(server.count("/anthropic"), 0, "still not sent");
        // a crash guard that can't write its state degrades to unguarded, it doesn't fail requests
        let ok: mlua::Value = h
            .lua()
            .load(r#"
mare.crash.begin = function() error("read-only state dir") end
local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
return LLM:stream({ model = m, messages = { { role = "user", content = { { type = "text", text = "other" } } } } }, function() end)
"#)
            .eval_async()
            .await
            .unwrap();
        let ok = mare_shared::lua::lua_to_json(h.lua(), ok).unwrap();
        assert!(ok.get("error").is_none_or(Value::is_null), "{ok}");
        assert_eq!(server.count("/anthropic"), 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn crash_guard_corruption_during_finish_and_release_notifies_user() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let guard = env.home.path().join("state/crash-guard.json");
        h.lua()
            .globals()
            .set(
                "corrupt_guard",
                h.lua()
                    .create_function(move |_, ()| std::fs::write(&guard, b"{corrupt").map_err(mlua::Error::external))
                    .unwrap(),
            )
            .unwrap();

        let notes: mlua::Value = h
            .lua()
            .load(
                r#"
local notes = {}
mare.ui.notify = function(text, level)
    notes[#notes + 1] = { text = tostring(text), level = tostring(level) }
end

local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))

-- 1. Corrupt crash-guard.json while a request is in flight -> discovered at `finish`.
local corrupted = false
local ok_res = LLM:stream({
    model = m,
    session_id = "s-finish",
    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
}, function()
    if not corrupted then
        corrupted = true
        corrupt_guard()
    end
end)
assert(not ok_res.error, tostring(ok_res.error))

-- 2. Corrupt crash-guard.json right before `release` at crash_limit.
local req_limit = {
    model = m,
    session_id = "s-release",
    messages = { { role = "user", content = { { type = "text", text = "boom" } } } },
}
local fp = LLM:fingerprint(req_limit)
for _ = 1, 3 do mare.crash.begin(fp) end
local orig_begin = mare.crash.begin
mare.crash.begin = function(key)
    local n, r = orig_begin(key)
    corrupt_guard()
    return n, r
end
local limit_res = LLM:stream(req_limit, function() end)
mare.crash.begin = orig_begin
assert(limit_res.error_kind == "crash_limit", tostring(limit_res.error_kind))

-- 3. Direct Lua bindings also return the quarantine backup path on finish and release.
mare.crash.begin("direct-finish")
corrupt_guard()
local finish_backup = mare.crash.finish("direct-finish")

mare.crash.begin("direct-release")
corrupt_guard()
local release_backup = mare.crash.release("direct-release")

return {
    notes = notes,
    finish_backup = finish_backup,
    release_backup = release_backup,
}
"#,
            )
            .eval_async()
            .await
            .unwrap();
        let out = mare_shared::lua::lua_to_json(h.lua(), notes).unwrap();
        let arr = out["notes"].as_array().expect("notes array");
        assert_eq!(arr.len(), 2, "both finish and release corruption must notify user: {out}");
        for (idx, n) in arr.iter().enumerate() {
            assert_eq!(n["level"], "warning", "note #{idx}: {n}");
            assert!(
                n["text"].as_str().unwrap_or("").contains("crash guard state was corrupt and reset"),
                "note #{idx}: {n}"
            );
        }
        assert!(out["finish_backup"].as_str().is_some_and(|s| s.contains("crash-guard.json.bad-")), "{out}");
        assert!(out["release_backup"].as_str().is_some_and(|s| s.contains("crash-guard.json.bad-")), "{out}");
        h.shutdown().await;
    })
    .await
}

/// Error events inside a 200 stream keep their code / type / status (Codex
/// sse/responses.rs:388-417): a 429 is a retried rate limit, an exhausted quota is final,
/// response.failed with server_is_overloaded counts against overloaded_retries.
#[tokio::test]
async fn mid_stream_error_events_are_classified_by_their_code() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let rate = compat_events(&[json!({"error":{"message":"slow down please","code":429}})]);
        let quota = compat_events(&[json!({"error":{"message":"no money","code":"insufficient_quota"}})]);
        server.route("/compat", vec![Resp::sse(rate), Resp::sse(fixture("openai-compat.sse")), Resp::sse(quota)]);
        let exhausted = "data: {\"error\":{\"code\":429,\"message\":\"Resource has been exhausted\",\"status\":\"RESOURCE_EXHAUSTED\"}}\n\n";
        server.route("/google", vec![Resp::sse(exhausted), Resp::sse(fixture("google.sse"))]);
        let overloaded = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        server.route("/anthropic", vec![Resp::sse(overloaded), Resp::sse(fixture("anthropic.sse"))]);
        let failed = responses_event(
            json!({"type":"response.failed","response":{"error":{"code":"server_is_overloaded","message":"busy"}}}),
        );
        server.route("/openai", vec![Resp::sse(failed)]);
        let h = provider_host(&mut env, &server, json!({})).await;

        assert_fixture_message("compat", &stream(&h, "compat/reasoner-model").await, 0);
        assert_eq!(server.count("/compat"), 2, "a mid-stream 429 is retried");
        let msg = ask(&h, "compat/chat-model").await;
        assert_eq!(msg["error_kind"], "quota", "{msg}");
        assert_eq!(server.count("/compat"), 3, "an exhausted quota is not retried");

        assert_fixture_message("google", &stream(&h, "google/gemini-2.5-pro").await, 0);
        assert_eq!(server.count("/google"), 2, "RESOURCE_EXHAUSTED mid-stream is a rate limit");

        assert_fixture_message("anthropic", &stream(&h, "anthropic/claude-sonnet-5-5").await, 5);
        assert_eq!(server.count("/anthropic"), 2);

        let msg = ask(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(msg["error_kind"], "overloaded", "{msg}");
        assert_eq!(server.count("/openai"), 4, "1 + overloaded_retries (3)");
        h.shutdown().await;
    })
    .await
}

/// Google: function calls without an id get distinct fallback ids; a blocked prompt is a
/// refusal (not retried); a 200 with a JSON body instead of SSE is read as the chunks.
#[tokio::test]
async fn google_missing_call_ids_blocked_prompts_and_json_bodies() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let two_calls = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"a\"}}}]}}]}\n\n\
                         data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"b\"}}}]},\"finishReason\":\"STOP\"}]}\n\n";
        let blocked = "data: {\"promptFeedback\":{\"blockReason\":\"SAFETY\"},\"usageMetadata\":{\"promptTokenCount\":3}}\n\n";
        let json_body = r#"[{"candidates":[{"content":{"parts":[{"text":"plain "}]}}]},{"candidates":[{"content":{"parts":[{"text":"json"},{"functionCall":{"name":"read","args":{"n":null,"a":[],"o":{}}}}]},"finishReason":"STOP"}]}]"#;
        server.route("/google", vec![Resp::sse(two_calls), Resp::sse(blocked), Resp::status(200, json_body)]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let msg = ask(&h, "google/gemini-2.5-pro").await;
        let ids: Vec<&str> = msg["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "tool_call")
            .map(|b| b["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 2, "{msg}");
        assert!(ids[0] != ids[1] && ids.iter().all(|i| i.starts_with("read_")), "{ids:?}");

        let msg = ask(&h, "google/gemini-2.5-pro").await;
        assert_eq!(msg["error_kind"], "refusal", "{msg}");
        assert!(msg["error"].as_str().unwrap().contains("SAFETY"), "{msg}");
        assert_eq!(server.count("/google"), 2, "a blocked prompt is not retried");

        let msg = ask(&h, "google/gemini-2.5-pro").await;
        assert_eq!(text_of(&msg), "plain json", "{msg}");
        let call = msg["content"].as_array().unwrap().iter().find(|b| b["type"] == "tool_call").unwrap();
        assert_eq!(call["arguments"], json!({"n":null,"a":[],"o":{}}));
        assert_eq!(server.count("/google"), 3);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn google_rejects_missing_and_non_object_tool_arguments_in_sse_and_json() {
    local(async {
        for as_sse in [true, false] {
            for (label, arguments) in [
                ("false", Some(json!(false))), ("null", Some(json!(null))),
                ("array", Some(json!([]))), ("string", Some(json!("{}"))),
            ] {
                let mut env = TestEnv::new().await;
                let server = MockServer::start().await;
                let mut function = json!({"name":"root_args_probe"});
                if let Some(value) = arguments { function["args"] = value; }
                let chunk = json!({"candidates":[{"content":{"parts":[{"functionCall":function}]},"finishReason":"STOP"}]});
                let first = if as_sse { Resp::sse(format!("data: {chunk}\n\n")) } else { Resp::status(200, chunk.to_string()) };
                let done = json!({"candidates":[{"content":{"parts":[{"text":"done"}]},"finishReason":"STOP"}]});
                server.route("/google", vec![first, Resp::sse(format!("data: {done}\n\n"))]);
                let h = provider_host(&mut env, &server, json!({ "model":"google/gemini-2.5-pro" })).await;
                expose(&h);
                exec(&h, r#"RAN, VOTES = 0, 0
                    T:register("root_args_probe", { description = "probe", parameters = { type = "object" }, execute = function() RAN = RAN + 1 return "ran" end })
                    C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "root_args_probe" then VOTES = VOTES + 1 end end)"#).await;
                let result = h.prompt("go", json!({})).await.unwrap();
                let (ran, votes): (i64, i64) = eval(&h, "return RAN, VOTES").await;
                assert_eq!((ran, votes), (0, 0), "{label}, sse={as_sse}: {result}");
                assert!(result.to_string().contains("arguments"), "{label}, sse={as_sse}: {result}");
                h.shutdown().await;
            }
        }
    }).await
}

#[tokio::test]
async fn streamed_function_calls_handle_empty_and_missing_json_arguments() {
    local(async {
        type Group<'a> = (&'a str, &'a str, &'a [(&'a str, bool)]);
        let groups: [Group<'_>; 3] = [
            ("openai/gpt-6.1-terra", "/openai", &[("missing", false), ("empty", false), ("object", true)]),
            (
                "compat/chat-model",
                "/compat",
                &[
                    ("missing", false),
                    ("empty", true),
                    ("object", true),
                    ("bad", false),
                    ("array", false),
                    ("null", false),
                    ("boolean", false),
                    ("required", false),
                ],
            ),
            (
                "anthropic/claude-sonnet-5-5",
                "/anthropic",
                &[
                    ("missing", false),
                    ("empty", true),
                    ("explicit", true),
                    ("bad", false),
                    ("array", false),
                    ("required", false),
                    ("nonempty", true),
                    ("initial", true),
                ],
            ),
        ];
        for (model, route, cases) in groups {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let mut responses = Vec::with_capacity(cases.len() * 2);
            for &(kind, explicit_empty) in cases {
                let events = if route == "/openai" {
                    let mut item = json!({"type":"function_call","id":"i","call_id":"call-1","name":"root_args_probe"});
                    if kind == "empty" { item["arguments"] = json!(""); }
                    if kind == "object" { item["arguments"] = json!("{}"); }
                    vec![json!({"type":"response.output_item.done", "item":item}), json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}})]
                } else if route == "/compat" {
                    let mut function = json!({"name":"root_args_probe"});
                    if kind == "empty" { function["arguments"] = json!(""); }
                    if kind == "object" { function["arguments"] = json!("{}"); }
                    if kind == "required" { function["arguments"] = json!(""); }
                    if kind == "bad" { function["arguments"] = json!("{"); }
                    if kind == "array" { function["arguments"] = json!("[]"); }
                    if kind == "null" { function["arguments"] = json!(null); }
                    if kind == "boolean" { function["arguments"] = json!(false); }
                    vec![json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":function}]}}]}), json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})]
                } else {
                    let mut block = json!({"type":"tool_use","id":"call-1","name":"root_args_probe"});
                    if explicit_empty || ["empty", "required", "bad", "array"].contains(&kind) { block["input"] = json!({}); }
                    if kind == "initial" { block["input"] = json!({"x":7}); }
                    let mut events = vec![
                        json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"claude","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}}),
                        json!({"type":"content_block_start","index":0,"content_block":block}),
                    ];
                    if ["empty", "required", "bad", "array", "nonempty", "initial"].contains(&kind) {
                        let fragment = match kind { "bad" => "{", "array" => "[]", "nonempty" => "{\"x\":1}", _ => "" };
                        events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":fragment}}));
                    }
                    events.extend([json!({"type":"content_block_stop","index":0}), json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}), json!({"type":"message_stop"})]);
                    events
                };
                let sse = events.iter().map(|event| {
                    if route == "/anthropic" { format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()) }
                    else { format!("data: {event}\n\n") }
                }).collect::<String>();
                let done = if route == "/openai" {
                    vec![json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}), json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}})]
                } else if route == "/compat" {
                    vec![json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]})]
                } else {
                    vec![json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"done"}}), json!({"type":"content_block_stop","index":0}), json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}), json!({"type":"message_stop"})]
                }.iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
                responses.push(Resp::sse(sse));
                responses.push(Resp::sse(done));
            }
            server.route(route, responses);
            let h = provider_host(&mut env, &server, json!({"model":model})).await;
            expose(&h);
            exec(&h, r#"RAN, VOTES = 0, 0
                C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "root_args_probe" then VOTES = VOTES + 1 end end)"#).await;
            for &(kind, explicit_empty) in cases {
                exec(&h, &format!(r#"if REMOVE_PROBE then REMOVE_PROBE() end
                    A:set_messages({{}})
                    RAN, VOTES, ARG_X, ARG_JSON = 0, 0, nil, nil
                    REMOVE_PROBE = T:register("root_args_probe", {{ description = "probe", parameters = {{ type = "object", required = {} }}, execute = function(args) RAN = RAN + 1 ARG_X = args.x ARG_JSON = mare.json.encode_strict(args) return "ran" end }})"#, if kind == "required" { "{\"x\"}" } else { "mare.json.array()" })).await;
                let result = h.prompt("go", json!({})).await.unwrap();
                let (ran, votes): (i64, i64) = eval(&h, "return RAN, VOTES").await;
                let expected = i64::from(explicit_empty);
                assert_eq!((ran, votes), (expected, expected), "{model}, {kind}: {result}");
                if route == "/compat" && kind == "empty" {
                    let received: String = eval(&h, "return ARG_JSON").await;
                    assert_eq!(serde_json::from_str::<Value>(&received).unwrap(), json!({}), "{model}: {received}");
                    assert_eq!(received, "{}", "compat empty arguments must reach the tool as an empty JSON object");
                }
                if kind == "nonempty" { assert_eq!(eval::<i64>(&h, "return ARG_X").await, 1); }
                if kind == "initial" { assert_eq!(eval::<i64>(&h, "return ARG_X").await, 7); }
                if !explicit_empty && kind != "required" { assert!(result.to_string().contains("arguments"), "{model}, {kind}: {result}"); }
            }
            assert_eq!(server.count(route), cases.len() * 2, "{model}: unexpected request count");
            h.shutdown().await;
        }
    }).await
}

#[tokio::test]
async fn google_text_events_skip_tool_argument_decoding() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let events = [
            json!({"candidates":[{"content":{"parts":[{"text":"one"}]}}]}),
            json!({"candidates":[{"content":{"parts":[{"text":"two"}]}}]}),
            json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"n":null,"a":[],"o":{}}}}]},"finishReason":"STOP"}]}),
        ];
        let sse = events.iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
        server.route("/google", vec![Resp::sse(sse)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        exec(&h, r#"LOSSLESS_DECODES = 0 local original = mare.json.decode_lossless
            mare.json.decode_lossless = function(...) LOSSLESS_DECODES = LOSSLESS_DECODES + 1 return original(...) end"#).await;
        let result = ask(&h, "google/gemini-2.5-pro").await;
        assert_eq!(text_of(&result), "onetwo");
        let call = result["content"].as_array().unwrap().iter().find(|v| v["type"] == "tool_call").unwrap();
        assert_eq!(call["arguments"], json!({"n":null,"a":[],"o":{}}));
        assert_eq!(eval::<i64>(&h, "return LOSSLESS_DECODES").await, 1, "only the tool-bearing event needs the second decode");
        h.shutdown().await;
    }).await
}

/// Malformed JSON inside an OpenAI Responses SSE event fails as
/// `malformed_response` without retrying.
#[tokio::test]
async fn responses_malformed_json_fails_without_retry() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let bad = "event: response.output_text.delta\ndata: {not valid json\n\n";
        server.route("/openai", vec![Resp::sse(bad)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = stream(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(msg["error_kind"], "malformed_response", "{msg}");
        assert_eq!(server.count("/openai"), 1, "malformed Responses JSON must not retry");
        h.shutdown().await;
    })
    .await
}

/// `exceeded_current_quota_error` classifies as non-retryable `quota`.
#[tokio::test]
async fn exceeded_current_quota_error_is_non_retryable_quota() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let body = r#"{"error":{"code":"exceeded_current_quota_error","message":"monthly spend cap reached"}}"#;
        server.route("/compat", vec![Resp::status(429, body)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = stream(&h, "compat/reasoner-model").await;
        assert_eq!(msg["error_kind"], "quota", "{msg}");
        assert_eq!(server.count("/compat"), 1, "quota error must not be retried");
        h.shutdown().await;
    })
    .await
}
