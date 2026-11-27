//! Remote compaction through the provider, and the local summary as its fallback.

use super::*;

#[tokio::test]
async fn remote_compaction_offloads_input_images_before_sending() {
    local(async {
        // a provider with a compaction endpoint: every input image becomes a durable attachment
        // reference before the request goes out
        let (called, sent, live) = remote_with_image(true).await;
        assert!(called, "the remote request was sent");
        assert!(sent.contains("[Attachment:") && !sent.contains(r#""type":"image""#), "{sent}");
        assert!(live.contains("[Attachment:"), "the completed offload remains after remote fails: {live}");
    })
    .await
}

/// A provider without remote compaction (Anthropic, Google, compat, faux): the remote stage
/// stops before preparing anything — the images stay inline, nothing is sent.
#[tokio::test]
async fn remote_compaction_keeps_images_inline_when_the_provider_has_none() {
    local(async {
        let (called, _, live) = remote_with_image(false).await;
        assert!(!called, "no remote request for an unsupported provider");
        assert!(live.contains(r#""type":"image""#) && !live.contains("[Attachment:"), "{live}");
    })
    .await
}

/// One image turn, compacted with only the remote stage; `supported` stands in for the
/// provider's compaction endpoint. → (compact_remote called, its messages, the context after)
async fn remote_with_image(supported: bool) -> (bool, String, String) {
    let env = TestEnv::new().await;
    let cfg = json!({ "plugins": { "compact": { "stages": ["remote"] } } });
    let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
    expose(&h);
    global(&h, "K", h.service_table("compact").unwrap());
    global(&h, "L", h.service_table("llm").unwrap());
    global(&h, "SUPPORTED", supported);
    let out: (bool, String, String) = eval(
        &h,
        r#"A:set_messages({
             { role = "user", content = {{ type = "image", mime = "image/png", data = mare.crypto.base64_encode("PNG") }} },
             { role = "assistant", content = {{ type = "text", text = "seen" }} },
           })
           if SUPPORTED then
             L.supports_remote_compact = function() return true end
           end
           local called, sent = false, nil
           L.compact_remote = function(_, req)
             called, sent = true, mare.json.encode(req.messages)
             return nil, "server down", "server"
           end
           pcall(K.run, K)
           return called, tostring(sent), mare.json.encode(A:list_messages().data)"#,
    )
    .await;
    h.shutdown().await;
    out
}

fn compact_history() -> &'static str {
    r#"A:set_messages({
        { role = "user", content = { { type = "text", text = "first question" } } },
		{ role = "assistant", content = { { type = "text", text = "first answer " .. string.rep("detail ", 500) } } },
        { role = "user", content = { { type = "text", text = "second question" } } },
        { role = "assistant", content = { { type = "text", text = "second answer" } } },
    })"#
}

const TEXT_SSE: &str = "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"## Summary\\nLOCAL-FALLBACK\"}]}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n";

async fn openai_host(env: &mut TestEnv, server: &MockServer) -> std::sync::Arc<mare_host::Host> {
    env.set_var("OPENAI_API_KEY", "test-openai");
    let h = env
        .host()
        .builtins(&["compact", "usage"])
        .config(json!({ "model": "openai/gpt-6.1-terra", "plugins": {
            "provider-openai": { "base_url": format!("{}/openai", server.base) },
            "llm": { "retry_base_ms": 1, "retries": 0 },
            "compact": { "keep_recent_tokens": 1 } } }))
        .no_session()
        .build()
        .await;
    expose(&h);
    exec(&h, compact_history()).await;
    global(&h, "K", h.service_table("compact").unwrap());
    h
}

#[tokio::test]
async fn remote_compaction_is_used_when_the_provider_has_it() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let items = json!({ "usage": { "input_tokens": 100, "output_tokens": 7,
            "input_tokens_details": { "cached_tokens": 20, "cache_write_tokens": 10 } }, "output": [
            { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "STALE-INSTRUCTIONS" }] },
            { "type": "reasoning", "encrypted_content": "STALE-REASONING" },
            { "type": "function_call", "call_id": "c9", "name": "ls", "arguments": "{}" },
            { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "second question" }] },
            { "type": "compaction", "encrypted_content": "ENC-COMPACTED" }
        ] });
        server.route("/openai/responses/compact", vec![Resp::status(200, items.to_string())]);
        server.route("/openai/responses", vec![Resp::sse(TEXT_SSE)]);
        let h = openai_host(&mut env, &server).await;
        global(&h, "U", h.service_table("usage").unwrap());
        let (ok, method): (bool, String) =
            eval(&h, "local r = K:run(); return r.status == 'compacted', tostring(A:list_messages().data[1].method)").await;
        assert!(ok);
        assert_eq!(method, "remote");
        assert_eq!(server.count("/openai/responses/compact"), 1);
        let (side, input, output, read, write): (i64, i64, i64, i64, i64) = eval(&h,
            "local u = U:totals(); return u.side_requests, u.input, u.output, u.cache_read, u.cache_write").await;
        assert_eq!((side, input, output, read, write), (1, 70, 7, 20, 10), "reported remote compaction usage");
        // the next request replays the server's compacted history
        h.prompt("third", json!({})).await.unwrap();
        let req = server.requests().into_iter().rev().find(|r| r.path == "/openai/responses").expect("a request");
        assert!(req.body.contains("ENC-COMPACTED"), "{}", req.body);
        assert!(!req.body.contains("first answer"), "compacted turns are not resent");
        // Codex should_keep_compacted_history_item: developer, reasoning and tool items are dropped
        for stale in ["STALE-INSTRUCTIONS", "STALE-REASONING", "\"c9\""] {
            assert!(!req.body.contains(stale), "{stale} must be filtered out: {}", req.body);
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn invalid_remote_compaction_still_counts_reported_usage() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let response = json!({ "output": [], "usage": { "input_tokens": 90, "output_tokens": 3,
            "input_tokens_details": { "cached_tokens": 20 } } });
        server.route("/openai/responses/compact", vec![Resp::status(200, response.to_string())]);
        let h = openai_host(&mut env, &server).await;
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "U", h.service_table("usage").unwrap());
        let (ok, kind, side, input, output, cached): (bool, String, i64, i64, i64, i64) = eval(
            &h,
            r#"
            local result, _, kind = L:compact_remote({ model = L:current(), system = "",
                messages = A:list_messages().data, tools = {} })
            local u = U:totals()
            return result ~= nil, tostring(kind), u.side_requests, u.input, u.output, u.cache_read"#,
        )
        .await;
        assert!(!ok && kind == "invalid");
        assert_eq!((side, input, output, cached), (1, 70, 3, 20));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn local_summary_is_the_fallback_when_remote_compaction_fails() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai/responses/compact", vec![Resp::status(500, r#"{"error":{"message":"boom"}}"#)]);
        server.route("/openai/responses", vec![Resp::sse(TEXT_SSE), Resp::sse(TEXT_SSE)]);
        let h = openai_host(&mut env, &server).await;
        let (ok, method, text): (bool, String, String) = eval(
            &h,
            "local r = K:run(); local m = A:list_messages().data; return r.status == 'compacted', tostring(m[1].method), mare.json.encode(m)",
        )
        .await;
        assert!(ok, "the local summary took over");
        assert_eq!(server.count("/openai/responses/compact"), 1, "remote was tried first");
        assert!(text.contains("LOCAL-FALLBACK"), "{text}");
        assert_ne!(method, "remote");
        h.shutdown().await;
    })
    .await
}

/// ... and skips the provider-side stage, which takes no instructions.
#[tokio::test]
async fn a_compact_focus_skips_remote_compaction() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai/responses/compact", vec![Resp::status(200, r#"{"output":[]}"#)]);
        server.route("/openai/responses", vec![Resp::sse(TEXT_SSE), Resp::sse(TEXT_SSE)]);
        let h = openai_host(&mut env, &server).await;
        assert!(eval::<bool>(&h, r#"return K:run("FOCUS").status == "compacted""#).await);
        assert_eq!(server.count("/openai/responses/compact"), 0, "remote never sees a focus it would ignore");
        h.shutdown().await;
    })
    .await
}

/// The provider yields while another task edits the same message table. Identity and count
/// remain unchanged, but installing the old response would lose the new content.
#[tokio::test]
async fn remote_compaction_discards_an_async_in_place_history_change() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([]))
            .config(json!({ "plugins": { "compact": { "stages": ["remote"] } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, compact_history()).await;
        let (ok, err, before, after, calls): (bool, String, String, String, i64) = eval(
            &h,
            r#"
            local requested, changed, calls = false, false, 0
            L.supports_remote_compact = function() return true end
            L.compact_remote = function()
                calls = calls + 1
                requested = true
                while not changed do mare.task.sleep(1) end
                return { provider = "openai", model = "test", items = {
                    { type = "compaction", encrypted_content = "STALE-REMOTE" }
                } }
            end
            local expected
            local editor = mare.task.spawn(function()
                while not requested do mare.task.sleep(1) end
                A:messages()[2].content[1].text = "NEW-CONTENT-IN-PLACE"
                expected = mare.json.encode(A:list_messages().data)
                changed = true
            end)
            local ok, err = pcall(K.run, K)
            editor:wait()
            return ok, tostring(err), expected, mare.json.encode(A:list_messages().data), calls
        "#,
        )
        .await;
        assert!(!ok && err.contains("conversation changed during compaction"), "{err}");
        assert_eq!(calls, 1, "the request really ran and yielded to the editor");
        assert_eq!(after, before, "the edited history remains intact");
        assert!(after.contains("NEW-CONTENT-IN-PLACE"));
        assert!(!after.contains("STALE-REMOTE"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn remote_compaction_rejects_unverifiable_input_before_requesting() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([]))
            .config(json!({ "plugins": { "compact": { "stages": ["remote"] } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, compact_history()).await;
        let (ok, err, same, after, called): (bool, String, bool, String, bool) = eval(
            &h,
            r#"
            local called = false
            L.supports_remote_compact = function() return true end
            L.compact_remote = function() called = true error("must not request") end
            -- Simulate a failed fingerprint primitive on the conversation snapshot; verification must fail closed.
            local hash = mare.crypto.sha256
            mare.crypto.sha256 = function(input)
                if type(input) == "string" and input:find("first answer", 1, true) then
                    error("fingerprint unavailable")
                end
                return hash(input)
            end
            local original = A:messages()[1]
            local ok, err = pcall(K.run, K)
            local same = A:messages()[1] == original
            mare.crypto.sha256 = hash
            local after = mare.json.encode(A:list_messages().data)
            return ok, tostring(err), same, after, called
        "#,
        )
        .await;
        assert!(!ok && err.contains("cannot verify the conversation"), "{err}");
        assert!(!called);
        assert!(same, "the original message table remains live");
        assert!(after.contains("first answer"));
        h.shutdown().await;
    })
    .await
}

/// Exercise the production session writer after the remote result's first stability check.
/// The gate is before append_compaction enters its staging/commit boundary, so the journal
/// must reject the obsolete summary as well as preserving the edited live message.
#[tokio::test]
async fn remote_compaction_revalidates_after_the_session_commit_call_yields() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([]))
            .config(json!({"plugins":{"compact":{"stages":["remote"]}}}))
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, compact_history()).await;
        let (accepted, error, live, summaries, path): (bool, String, String, i64, String) = eval(
            &h,
            r#"
            local messages=A:messages()
            A:set_messages({})
            for _,message in ipairs(messages) do A:append(message) end
            assert(S:info().path, 'production session file must be open')
            L.supports_remote_compact=function() return true end
            L.compact_remote=function() return {provider='openai',model='test',items={
                {type='compaction',encrypted_content='STALE-SESSION-SUMMARY'}
            }} end
            local entered_tx,entered_rx=mare.task.channel()
            local release_tx,release_rx=mare.task.channel()
            local original=S.append_compaction
            S.append_compaction=function(...)
                entered_tx:send(true)
                release_rx:recv()
                return original(...)
            end
            local result_tx,result_rx=mare.task.channel()
            local task=mare.task.spawn(function() result_tx:send(table.pack(pcall(K.run, K))) end)
            entered_rx:recv()
            A:messages()[2].content[1].text='EDITED-AFTER-INITIAL-VALIDATION'
            release_tx:send(true)
            local result=result_rx:recv()
            task:wait()
            S.append_compaction=original
            local summaries=0
            for _,entry in ipairs(S:entries()) do if entry.type=='compaction' then summaries=summaries+1 end end
            return result[1]==true,tostring(result[2]),mare.json.encode(A:list_messages().data),summaries,S:info().path
        "#,
        )
        .await;
        assert!(!accepted, "obsolete summary was accepted");
        assert!(error.contains("stale") || error.contains("changed"), "{error}");
        assert!(live.contains("EDITED-AFTER-INITIAL-VALIDATION"), "{live}");
        assert!(!live.contains("STALE-SESSION-SUMMARY"), "{live}");
        assert_eq!(summaries, 0, "a refused summary must not enter the durable journal");
        let journal = std::fs::read_to_string(path).unwrap();
        for line in journal.lines() {
            let entry: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_ne!(entry["type"], "compaction", "a refused summary reached the session file");
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn remote_compaction_rejects_missing_compaction_type_and_gates_model_and_ciphertext_on_replay() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let bad_items = json!({
            "output": [{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "just user" }] }]
        });
        server.route("/openai/responses/compact", vec![Resp::status(200, bad_items.to_string())]);
        server.route("/openai/responses", vec![Resp::sse(TEXT_SSE), Resp::sse(TEXT_SSE)]);
        let h = openai_host(&mut env, &server).await;
        let (ok_bad, err_bad, all_msgs): (bool, String, String) = eval(
            &h,
            "local ok, err = pcall(K.run, K); return ok and type(err) == 'table' and err.status == 'compacted', mare.json.encode(err), mare.json.encode(A:list_messages().data)",
        )
        .await;
        assert!(ok_bad, "remote rejection falls back to local cleanly: err={err_bad}, msgs={all_msgs}");
        assert!(all_msgs.contains("\"method\":\"local\"") && all_msgs.contains("LOCAL-FALLBACK"), "missing type=='compaction' must reject remote item and fall back to local: {all_msgs}");

        exec(
            &h,
            r#"
            A:set_messages({
                {
                    role = "user",
                    compaction = true,
                    content = { { type = "text", text = "PORTABLE-FALLBACK-TEXT" } },
                    remote = {
                        provider_id = "openai",
                        model_id = "other-openai-model",
                        items = { { type = "compaction", encrypted_content = "SHOULD-NOT-REPLAY-MODEL-MISMATCH" } },
                    },
                },
            })
            "#,
        )
        .await;
        h.prompt("next1", json!({})).await.unwrap();
        let reqs = server.requests();
        let body1 = &reqs.iter().rev().find(|r| r.path == "/openai/responses").unwrap().body;
        assert!(!body1.contains("SHOULD-NOT-REPLAY-MODEL-MISMATCH"), "wrong model must not replay remote.items: {body1}");
        assert!(body1.contains("PORTABLE-FALLBACK-TEXT"), "must fall back to portable text summary: {body1}");

        exec(
            &h,
            r#"
            A:set_messages({
                {
                    role = "user",
                    compaction = true,
                    content = { { type = "text", text = "PORTABLE-HUGE-CIPHERTEXT" } },
                    remote = {
                        provider_id = "openai",
                        model_id = "gpt-6.1-terra",
                        items = { { type = "compaction", encrypted_content = string.rep("A", 9500001) } },
                    },
                },
            })
            "#,
        )
        .await;
        h.prompt("next2", json!({})).await.unwrap();
        let reqs2 = server.requests();
        let body2 = &reqs2.iter().rev().find(|r| r.path == "/openai/responses").unwrap().body;
        assert!(body2.contains("PORTABLE-HUGE-CIPHERTEXT"), "oversized ciphertext must fall back to portable text: {body2}");
        h.shutdown().await;
    })
    .await
}

/// Remote compaction falls back to local summary ONLY on unsupported/server/transport/invalid;
/// auth, rate_limit, and payload_too_large stop compaction and report an error.
#[tokio::test]
async fn remote_compaction_only_falls_back_on_allowlisted_error_kinds() {
    local(async {
        for (status, label) in [(401u16, "auth"), (429, "rate_limit"), (413, "payload_too_large")] {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            server.route("/openai/responses/compact", vec![Resp::status(status, format!(r#"{{"error":"{label}"}}"#))]);
            server.route("/openai/responses", vec![Resp::sse(TEXT_SSE)]);
            let h = openai_host(&mut env, &server).await;
            let (ok, err, msgs): (bool, String, String) = eval(
                &h,
                "local ok, err = pcall(K.run, K); return ok and type(err) == 'table' and err.status == 'compacted', mare.json.encode(err), mare.json.encode(A:list_messages().data)",
            )
            .await;
            assert!(!ok, "remote {status} ({label}) must stop compaction instead of falling back: err={err}, msgs={msgs}");
            let local_hits = server.requests().iter().filter(|r| r.path == "/openai/responses").count();
            assert_eq!(local_hits, 0, "local summary must not run after remote {status} ({label})");
            assert!(!msgs.contains("LOCAL-FALLBACK"), "local summary must not commit after remote {status} ({label}): {msgs}");
            h.shutdown().await;
        }

        // Allowlisted kinds (server=500, invalid=400) MUST still fall back to local summary.
        for (status, label) in [(500u16, "server"), (400, "invalid")] {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            server.route("/openai/responses/compact", vec![Resp::status(status, format!(r#"{{"error":"{label}"}}"#))]);
            server.route("/openai/responses", vec![Resp::sse(TEXT_SSE)]);
            let h = openai_host(&mut env, &server).await;
            let (ok, err, msgs): (bool, String, String) = eval(
                &h,
                "local ok, err = pcall(K.run, K); return ok and type(err) == 'table' and err.status == 'compacted', mare.json.encode(err), mare.json.encode(A:list_messages().data)",
            )
            .await;
            assert!(ok, "remote {status} ({label}) must fall back to local summary: err={err}");
            assert!(msgs.contains("LOCAL-FALLBACK"), "expected local fallback for remote {status} ({label}): {msgs}");
            h.shutdown().await;
        }
    })
    .await
}
