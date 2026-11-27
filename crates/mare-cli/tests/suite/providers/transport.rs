//! Transport: request and event hooks, proxy and CA settings, WebSocket and fresh connections.

use super::*;

/// extension points: configured headers, `provider/request` rewrites the wire request,
/// `provider/transport` can take it over (the WebSocket transport plugs in here), and
/// `provider/event` sees raw stream events
#[tokio::test]
async fn openai_request_transport_and_event_hooks() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(fixture("openai-responses.sse"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-openai": { "headers": { "x-org": "acme" } } } }),
        )
        .await;
        expose(&h);
        exec(
            &h,
            r#"
            EVENTS = 0
            C.cordis:on("provider/request", function(w, ev) w.headers["x-hooked"] = "1"; return ev.next() end)
            C.cordis:on("provider/event", function(data, ev) EVENTS = EVENTS + 1; return ev.next() end)
            "#,
        )
        .await;
        openai_call(&h).await.unwrap();
        let req = &server.requests()[0];
        assert_eq!(req.header("x-org"), Some("acme"));
        assert_eq!(req.header("x-hooked"), Some("1"));
        assert!(eval::<i64>(&h, "return EVENTS").await > 0, "provider/event saw the stream");
        // a transport plugin answers instead of HTTP (replays the recorded stream, text marked)
        global(&h, "SSE", fixture("openai-responses.sse"));
        exec(
            &h,
            r#"
            C.cordis:on("provider/transport", function(t)
                VIA_TRANSPORT = t.request and t.request.headers["x-hooked"] == "1"
                local evs = {}
                for block in (SSE .. "\n\n"):gmatch("(.-)\n\n") do
                    local data = block:match("data: ([^\n]*)")
                    if data then evs[#evs + 1] = { data = data } end
                end
                local i = 0
                return { ok = true, status = 200, headers = {}, next_event = function() i = i + 1; return evs[i] end }
            end)
            "#,
        )
        .await;
        let res = openai_call(&h).await.unwrap();
        assert!(res.to_string().contains("Reading it now."), "{res}");
        assert!(eval::<bool>(&h, "return VIA_TRANSPORT == true").await, "transport got the rewritten request");
        assert_eq!(server.requests().len(), 1, "the transport took the request; no HTTP call");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn proxy_and_ca_misconfiguration_are_clear_errors() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("ALL_PROXY", "socks5://127.0.0.1:1080");
        let h = env.host().no_session().build().await;
        let err: String = eval(&h, r#"local ok, e = pcall(mare.http.request, { url = "http://127.0.0.1:9/", fresh = true }); return tostring(e)"#).await;
        assert!(err.contains("SOCKS proxies are not supported"), "{err}");
        h.shutdown().await;
        env.set_var("ALL_PROXY", "");
        env.set_var("MARE_CA_CERTIFICATE", env.cwd.join("missing.pem"));
        let h = env.host().no_session().build().await;
        let err: String = eval(&h, r#"local ok, e = pcall(mare.http.request, { url = "http://127.0.0.1:9/", fresh = true }); return tostring(e)"#).await;
        assert!(err.contains("MARE_CA_CERTIFICATE") && err.contains("PEM file"), "{err}");
        h.shutdown().await;
    })
    .await
}

/// What [`ws_server`] saw: connections, every `response.create` frame, each handshake's
/// `openai-beta` header, and per connection its handshake `authorization` and frame count.
type WsSeen = (usize, Vec<Value>, Vec<String>, Vec<(String, usize)>);

/// A local Responses WebSocket server: counts connections, records each `response.create`
/// frame, the handshake's `openai-beta` and `authorization` headers and the frames per
/// connection, and answers every frame with the events of the SSE fixture as text frames (the
/// connection stays open, like the real endpoint).
async fn ws_server() -> (u16, Arc<std::sync::Mutex<WsSeen>>) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(std::sync::Mutex::new((0usize, vec![], vec![], vec![])));
    let events: Vec<String> =
        fixture("openai-responses.sse").lines().filter_map(|l| l.strip_prefix("data: ").map(str::to_string)).collect();
    let s2 = seen.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let seen = s2.clone();
            let events = events.clone();
            tokio::spawn(async move {
                let beta = Arc::new(std::sync::Mutex::new(String::new()));
                let auth = Arc::new(std::sync::Mutex::new(String::new()));
                let (b2, a2) = (beta.clone(), auth.clone());
                #[allow(clippy::result_large_err)] // the signature tungstenite's callback requires
                let cb =
                    move |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                          resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                        let header = |name: &str| {
                            req.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
                        };
                        *b2.lock().unwrap() = header("openai-beta");
                        *a2.lock().unwrap() = header("authorization");
                        Ok(resp)
                    };
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(sock, cb).await else { return };
                let conn = {
                    let mut g = seen.lock().unwrap();
                    g.0 += 1;
                    g.2.push(beta.lock().unwrap().clone());
                    g.3.push((auth.lock().unwrap().clone(), 0));
                    g.3.len() - 1
                };
                while let Some(Ok(msg)) = ws.next().await {
                    let Message::Text(t) = msg else { continue };
                    {
                        let mut g = seen.lock().unwrap();
                        g.1.push(serde_json::from_str(t.as_str()).unwrap());
                        g.3[conn].1 += 1;
                    }
                    for e in &events {
                        if ws.send(Message::Text(e.clone().into())).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    (port, seen)
}

/// Responses over one WebSocket (Codex responses_websocket.rs): `response.create`
/// frames with the Responses body, the beta header on the handshake, the connection reused by
/// the next turn instead of a new handshake.
#[tokio::test]
async fn openai_websocket_transport_reuses_one_connection_across_turns() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let (port, seen) = ws_server().await;
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-openai": {
                "base_url": format!("http://127.0.0.1:{port}/openai"), "transport": "websocket" } } }),
        )
        .await;
        let a = openai_call(&h).await.unwrap();
        let b = openai_call(&h).await.unwrap();
        assert_eq!(a["stop_reason"], b["stop_reason"]);
        assert!(!a["content"].as_array().unwrap().is_empty(), "{a}");
        let g = seen.lock().unwrap().clone();
        assert_eq!(g.0, 1, "one connection for both turns");
        assert_eq!(g.1.len(), 2);
        assert_eq!(g.1[0]["type"], "response.create");
        assert_eq!(g.1[0]["model"], "gpt-6.1-terra");
        assert!(g.1[0]["input"].is_array());
        assert_eq!(g.2[0], "responses_websockets=2026-02-06");
        assert!(server.requests().is_empty(), "nothing went over HTTP");
        h.shutdown().await;
    })
    .await
}

/// A WebSocket belongs to the credential it was opened with (provider corpus; Codex
/// core/tests/suite/client_websockets.rs:1103, pi openai-codex-stream.test.ts:1520): after the
/// API key changes, the next turn opens a new handshake carrying the new authorization, and the
/// connection opened with the old key gets no further `response.create` frame — a turn must
/// never run under another account's connection.
#[tokio::test]
async fn openai_websocket_opens_a_new_connection_when_the_credential_changes() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let (port, seen) = ws_server().await;
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-openai": {
                "base_url": format!("http://127.0.0.1:{port}/openai"), "transport": "websocket" } } }),
        )
        .await;
        env.set_var("OPENAI_API_KEY", "KEY-ONE");
        openai_call(&h).await.unwrap();
        env.set_var("OPENAI_API_KEY", "KEY-TWO");
        openai_call(&h).await.unwrap();
        h.shutdown().await;
        let g = seen.lock().unwrap().clone();
        let conns: Vec<(String, usize)> = g.3.clone();
        assert_eq!(conns.len(), 2, "a new handshake after the credential changed: {conns:?}");
        assert_eq!(conns[0].0, "Bearer KEY-ONE", "{conns:?}");
        assert_eq!(conns[1].0, "Bearer KEY-TWO", "{conns:?}");
        assert_eq!(
            conns.iter().map(|c| c.1).collect::<Vec<_>>(),
            [1, 1],
            "a frame went over the old account's connection"
        );
        assert!(server.requests().is_empty(), "nothing went over HTTP");
    })
    .await
}

/// PROV-EOF regression: an in-stream `error` event without `status`/`status_code` must still
/// invalidate and close the cached WebSocket connection; otherwise trailing frames left on the
/// open socket would be consumed by the next turn (Codex responses_error_tests.rs:112,
/// client_websockets.rs:2599).
#[tokio::test]
async fn openai_websocket_in_stream_error_without_status_closes_connection_before_next_turn() {
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_tungstenite::tungstenite::Message;

    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let conns = Arc::new(AtomicUsize::new(0));
        let events: Vec<String> =
            fixture("openai-responses.sse").lines().filter_map(|l| l.strip_prefix("data: ").map(str::to_string)).collect();
        let conns_srv = conns.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let id = conns_srv.fetch_add(1, Ordering::SeqCst);
                let events = events.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await else { return };
                    while let Some(Ok(msg)) = ws.next().await {
                        if !matches!(msg, Message::Text(_)) {
                            continue;
                        }
                        if id == 0 {
                            let _ = ws
                                .send(Message::Text(
                                    r#"{"type":"error","code":"invalid_request_error","message":"boom"}"#.into(),
                                ))
                                .await;
                            let _ = ws
                                .send(Message::Text(
                                    r#"{"type":"response.output_text.delta","item_id":"stale","delta":"STALE_TRAILING"}"#
                                        .into(),
                                ))
                                .await;
                            let _ = ws.flush().await;
                            // Keep the WebSocket open; wait until the client closes or drops it.
                            while let Some(Ok(_)) = ws.next().await {}
                            return;
                        } else {
                            for e in &events {
                                if ws.send(Message::Text(e.clone().into())).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-openai": {
                "base_url": format!("http://127.0.0.1:{port}/openai"), "transport": "websocket" } } }),
        )
        .await;
        let err = openai_call(&h).await.unwrap_err();
        assert!(err.contains("boom"), "expected in-stream error, got: {err}");
        let ok = openai_call(&h).await.unwrap();
        let text = ok["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "text")
            .map(|b| b["text"].as_str().unwrap_or(""))
            .unwrap_or("");
        assert_eq!(text, "Reading it now.", "next turn must not see stale trailing frame: {ok}");
        assert_eq!(conns.load(Ordering::SeqCst), 2, "must open a fresh WebSocket after in-stream error");
        h.shutdown().await;
    })
    .await
}

/// a WebSocket that can't connect (no upgrade) falls back to HTTP streaming,
/// and the rest of the session stays on HTTP (no second handshake attempt).
#[tokio::test]
async fn openai_websocket_falls_back_to_http_when_the_upgrade_fails() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/openai",
            vec![
                Resp::status(404, "no websockets here"),
                Resp::sse(fixture("openai-responses.sse")),
                Resp::sse(fixture("openai-responses.sse")),
            ],
        );
        let h =
            provider_host(&mut env, &server, json!({ "plugins": { "provider-openai": { "transport": "websocket" } } }))
                .await;
        openai_call(&h).await.unwrap();
        openai_call(&h).await.unwrap();
        let reqs = server.requests();
        let upgrades = reqs.iter().filter(|r| r.header("upgrade").is_some()).count();
        assert_eq!(upgrades, 1, "one handshake attempt, then HTTP for the session");
        assert_eq!(reqs.len(), 3, "the failed handshake + two HTTP streams");
        h.shutdown().await;
    })
    .await
}

/// extension points on every provider (not only OpenAI): the `headers` option, the
/// `provider/request` waterfall (here it injects a header) and `provider/event` (raw events).
#[tokio::test]
async fn anthropic_google_and_compat_share_the_request_and_event_hooks_and_headers() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic.sse"))]);
        server.route("/google", vec![Resp::sse(fixture("google.sse"))]);
        server.route("/compat", vec![Resp::sse(fixture("openai-compat.sse"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": {
                "provider-anthropic": { "headers": { "x-team": "blue" } },
                "provider-google": { "headers": { "x-team": "blue" } },
                "provider-openai-compat": { "headers": { "x-team": "blue" } },
            } }),
        )
        .await;
        expose(&h);
        let counts: Vec<i64> = eval(
            &h,
            r#"local seen = {}
               C.cordis:on("provider/request", function(w, ev) w.headers["x-hooked"] = w.provider; return ev.next() end)
               C.cordis:on("provider/event", function(data, ev) seen[data.provider] = (seen[data.provider] or 0) + 1; return ev.next() end)
               local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
               for _, ref in ipairs({ "anthropic/claude-sonnet-5-5", "google/gemini-2.5-pro", "deepseek/deepseek-reasoner" }) do
                 local m = assert(LLM:resolve(ref))
                 LLM:stream({ model = m, system = "S", messages = msgs }, function() end)
               end
               return { seen.anthropic or 0, seen.google or 0, seen.deepseek or 0 }"#,
        )
        .await;
        assert!(counts.iter().all(|n| *n > 0), "provider/event fired for every provider: {counts:?}");
        let reqs = server.requests();
        assert_eq!(reqs.len(), 3);
        for (r, prov) in reqs.iter().zip(["anthropic", "google", "deepseek"]) {
            assert_eq!(r.header("x-team"), Some("blue"), "{}: headers option", r.path);
            assert_eq!(r.header("x-hooked"), Some(prov), "{}: provider/request rewrote it", r.path);
        }
        h.shutdown().await;
    })
    .await
}

/// A transport failure (stream cut) retries on a new connection, never a pooled one; a rate
/// limit retry keeps the pool.
#[tokio::test]
async fn a_transport_retry_uses_a_fresh_connection() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let full = fixture("anthropic.sse");
        let rate = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
        server.route(
            "/anthropic",
            vec![
                Resp::sse(full.clone()).cut(full.len() / 2),
                Resp::sse(full.clone()),
                Resp::status(429, rate).header("retry-after", "0"),
                Resp::sse(full),
            ],
        );
        let h = provider_host(&mut env, &server, json!({})).await;
        exec(&h, SPY_HTTP).await;
        assert_fixture_message("anthropic", &stream(&h, "anthropic/claude-sonnet-5-5").await, 5);
        assert_fixture_message("anthropic", &stream(&h, "anthropic/claude-sonnet-5-5").await, 5);
        let fresh: Vec<bool> = eval::<mlua::Table>(&h, "return SEEN")
            .await
            .sequence_values::<mlua::Table>()
            .map(|t| t.unwrap().get("fresh").unwrap())
            .collect();
        assert_eq!(fresh, [false, true, false, false], "only the retry after the cut stream is fresh");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn provider_request_hooks_cannot_change_the_frozen_tool_declarations() {
    local(async {
        for model in
            ["openai/gpt-6.1-terra", "anthropic/claude-sonnet-5-5", "deepseek/deepseek-chat", "google/gemini-2.5-pro"]
        {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let h = provider_host(&mut env, &server, json!({})).await;
            expose(&h);
            exec(&h, r#"C.cordis:on("provider/request", function(w, next) w.body.tools = {}; return next() end)"#)
                .await;
            let response = stream(&h, model).await;
            assert_eq!(response["stop_reason"], "error", "{model}: {response}");
            assert!(
                response.to_string().contains("provider request changed frozen tool declarations"),
                "{model}: {response}"
            );
            assert!(server.requests().is_empty(), "{model} sent a changed declaration");
            h.shutdown().await;
        }
    })
    .await
}

/// `mare.tool.describe` / `tool_descriptions` reach the model: the outgoing OpenAI Responses request
/// carries the user's description for a function tool (bash) and for a custom (freeform) tool
/// (apply_patch, whose encoder prefers `freeform.description`, provider-openai/wire.lua:348).
#[tokio::test]
async fn user_tool_descriptions_are_sent_for_function_and_custom_tools() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let done = [
            json!({"type":"response.output_item.done", "item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}),
            json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}),
        ].iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
        server.route("/openai", vec![Resp::sse(done)]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "model": "openai/gpt-6.1-terra", "tool_descriptions": {
                "bash": "user bash description", "apply_patch": "user patch description" } }),
        )
        .await;
        let result = h.prompt("go", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let requests = server.requests();
        let tools = requests[0].json()["tools"].clone();
        let tool = |name: &str| {
            tools.as_array().unwrap().iter().find(|t| t["name"] == name).cloned().unwrap_or_else(|| panic!("no {name} in {tools}"))
        };
        assert_eq!(tool("bash")["type"], "function", "{tools}");
        assert_eq!(tool("bash")["description"], "user bash description", "{tools}");
        assert_eq!(tool("apply_patch")["type"], "custom", "{tools}");
        assert_eq!(tool("apply_patch")["description"], "user patch description", "{tools}");
        // only the description is replaced: the grammar is the tool's own
        assert_eq!(tool("apply_patch")["format"]["syntax"], "lark", "{tools}");
        assert!(tool("apply_patch")["format"]["definition"].as_str().is_some_and(|d| d.contains("Begin Patch")), "{tools}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_custom_grammar_is_preserved_and_wrapped_input_is_validated() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let events = [
            json!({"type":"response.output_item.added", "item":{"id":"c1","type":"custom_tool_call","call_id":"call_1","name":"grammar_probe"}}),
            json!({"type":"response.output_item.done", "item":{"id":"c1","type":"custom_tool_call","call_id":"call_1","name":"grammar_probe","input":"bad"}}),
            json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}),
        ];
        let sse = events.iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
        let end = [
            json!({"type":"response.output_item.done", "item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}),
            json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}),
        ].iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
        server.route("/openai", vec![Resp::sse(sse), Resp::sse(end)]);
        let h = provider_host(&mut env, &server, json!({ "model":"openai/gpt-6.1-terra" })).await;
        expose(&h);
        exec(&h, r#"RAN, VOTED = 0, 0
            T:register("grammar_probe", { description = "probe", freeform = { syntax = "lark", definition = "start: /.+/" },
              parameters = { type = "object", properties = { input = { const = "good" } }, required = { "input" } },
              execute = function() RAN = RAN + 1 return "ran" end })
            C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "grammar_probe" then VOTED = VOTED + 1 end end)"#).await;
        let result = h.prompt("go", json!({})).await.unwrap();
        let (ran, voted): (i64, i64) = eval(&h, "return RAN, VOTED").await;
        assert_eq!((ran, voted), (0, 0), "{result}");
        assert!(result.to_string().contains("Invalid arguments"), "{result}");
        let requests = server.requests();
        let first = requests[0].json();
        let declaration = first["tools"].as_array().unwrap().iter().find(|t| t["name"] == "grammar_probe").unwrap();
        assert_eq!(declaration["type"], "custom");
        assert_eq!(declaration["format"]["syntax"], "lark");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn legacy_function_declarations_cannot_be_injected_by_a_request_hook() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;
        expose(&h);
        exec(
            &h,
            r#"C.cordis:on("provider/request", function(w, next)
            w.body.functions = { { name = "hidden", parameters = { type = "object" } } }
            return next()
        end)"#,
        )
        .await;
        let response = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(response["stop_reason"], "error", "{response}");
        assert!(response.to_string().contains("provider request changed frozen tool declarations"), "{response}");
        assert!(server.requests().is_empty());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_custom_grammar_missing_input_is_not_defaulted_to_empty_string() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let events = [
            json!({"type":"response.output_item.done", "item":{"id":"c1","type":"custom_tool_call","call_id":"call_1","name":"grammar_missing"}}),
            json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}),
        ];
        let sse = events.iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
        let end = [
            json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}),
            json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}),
        ].iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
        server.route("/openai", vec![Resp::sse(sse), Resp::sse(end)]);
        let h = provider_host(&mut env, &server, json!({"model":"openai/gpt-6.1-terra"})).await;
        expose(&h);
        exec(&h, r#"RAN, VOTES = 0, 0
            T:register("grammar_missing", { description = "probe", freeform = { syntax = "lark", definition = "start: /.*/" },
              parameters = { type = "object", properties = { input = { type = "string" } }, required = { "input" } },
              execute = function() RAN = RAN + 1 return "ran" end })
            C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "grammar_missing" then VOTES = VOTES + 1 end end)"#).await;
        let result = h.prompt("go", json!({})).await.unwrap();
        let (ran, votes): (i64, i64) = eval(&h, "return RAN, VOTES").await;
        assert_eq!((ran, votes), (0, 0), "{result}");
        assert!(result.to_string().contains("custom tool input must be a string"), "{result}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn provider_requests_and_model_discovery_block_cross_origin_redirects() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let attacker = MockServer::start().await;
        let base = server.base.clone();
        server.route(
            "/openai/responses",
            vec![
                Resp::status(302, "").header("location", &format!("{}/steal-stream", attacker.base)),
                Resp::status(302, "").header("location", &format!("{}/steal-stream", attacker.base)),
            ],
        );
        server.route(
            "/compat/models",
            vec![
                Resp::status(302, "").header("location", &format!("{}/steal-models", attacker.base)),
                Resp::status(302, "").header("location", &format!("{}/steal-models", attacker.base)),
            ],
        );
        server.route(
            "/codex/codex/models",
            vec![
                Resp::status(302, "").header("location", &format!("{}/steal-codex-models", attacker.base)),
                Resp::status(302, "").header("location", &format!("{}/steal-codex-models", attacker.base)),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-openai": {
                        "codex_base_url": format!("{base}/codex"),
                        "auth_base_url": format!("{base}/auth"),
                        "headers": { "x-org": "acme" }
                    },
                    "provider-openai-compat": { "headers": { "x-team": "blue" } },
                }
            }),
        )
        .await;

        let stream_res = stream(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(stream_res["stop_reason"], "error", "{stream_res}");
        assert!(stream_res.to_string().contains("redirect"), "{stream_res}");

        let (ok_models, err_models): (bool, String) = eval(
            &h,
            r#"
            local ok, res = pcall(function() return LLM:refresh_models("deepseek", { force = true }) end)
            return ok, tostring(res)
            "#,
        )
        .await;
        assert!(!ok_models, "list_models must reject cross-origin redirect");
        assert!(err_models.contains("redirect"), "{err_models}");

        // ChatGPT OAuth (list_models_codex) model discovery must also block cross-origin redirects
        // after sending Authorization, chatgpt-account-id, and custom headers to the origin.
        let (ok_codex, err_codex): (bool, String) = eval(
            &h,
            r#"
            mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
              ["openai-codex"] = { type = "oauth", access = "sub-tok", refresh = "sub-ref", expires = 9999999999999, account_id = "acct-sub" }
            }), { mode = 384 })
            local ok, res = pcall(function() return LLM:refresh_models("openai", { force = true }) end)
            return ok, tostring(res)
            "#,
        )
        .await;
        assert!(!ok_codex, "list_models_codex must reject cross-origin redirect");
        assert!(err_codex.contains("redirect"), "{err_codex}");

        let codex_req = server
            .requests()
            .into_iter()
            .find(|r| r.path.starts_with("/codex/codex/models"))
            .expect("origin server must receive /codex/codex/models");
        assert_eq!(codex_req.header("chatgpt-account-id"), Some("acct-sub"));
        assert_eq!(codex_req.header("x-org"), Some("acme"));
        assert_eq!(codex_req.header("authorization"), Some("Bearer sub-tok"));
        assert!(attacker.requests().is_empty(), "cross-origin target received requests: {:?}", attacker.requests());
        h.shutdown().await;
    })
    .await
}

/// Ref: Codex `oauth` token + revoke redirect blocking.

#[tokio::test]
async fn oauth_token_and_revoke_block_cross_origin_307_308_redirects() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let attacker = MockServer::start().await;
        let base = server.base.clone();
        server.route(
            "/auth/oauth/token",
            vec![
                Resp::status(307, "").header("location", &format!("{}/steal-token", attacker.base)),
                Resp::status(307, "").header("location", &format!("{}/steal-token", attacker.base)),
            ],
        );
        server.route(
            "/auth/oauth/revoke",
            vec![
                Resp::status(308, "").header("location", &format!("{}/steal-revoke", attacker.base)),
                Resp::status(308, "").header("location", &format!("{}/steal-revoke", attacker.base)),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-openai": {
                        "codex_base_url": format!("{base}/codex"),
                        "auth_base_url": format!("{base}/auth"),
                    }
                }
            }),
        )
        .await;

        exec(
            &h,
            r#"
            mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
              ["openai-codex"] = { type = "oauth", access = "old", refresh = "rt-secret", expires = 0, account_id = "acct-sub" }
            }), { mode = 384 })
            "#,
        )
        .await;

        let refresh_res = stream(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(refresh_res["stop_reason"], "error", "{refresh_res}");
        assert!(refresh_res.to_string().contains("redirect"), "{refresh_res}");

        let token_req = server
            .requests()
            .into_iter()
            .find(|r| r.path.starts_with("/auth/oauth/token"))
            .expect("origin server must receive /auth/oauth/token");
        assert!(token_req.body.contains("rt-secret"), "origin received refresh_token");

        let logout_raw: String = eval(&h, r#"return mare.json.encode(LLM:logout("openai"))"#).await;
        let logout_out: Value = serde_json::from_str(&logout_raw).expect("valid json from LLM:logout");
        let warnings = logout_out["warnings"].as_array().expect("warnings array");
        assert!(
            warnings.iter().any(|w| w.as_str().is_some_and(|s| s.contains("redirect"))),
            "expected revoke redirect warning in {logout_out}"
        );
        let revoke_req = server
            .requests()
            .into_iter()
            .find(|r| r.path.starts_with("/auth/oauth/revoke"))
            .expect("origin server must receive /auth/oauth/revoke");
        assert!(revoke_req.body.contains("rt-secret"), "origin received revoke token");

        assert!(attacker.requests().is_empty(), "cross-origin target received requests: {:?}", attacker.requests());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn oauth_code_exchange_and_device_polling_block_cross_origin_redirects() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let attacker = MockServer::start().await;
        let base = server.base.clone();
        server.route(
            "/auth/api/accounts/deviceauth/usercode",
            vec![
                Resp::status(
                    200,
                    json!({ "device_auth_id": "dev-1", "user_code": "CODE-1", "interval": 1 }).to_string(),
                ),
                Resp::status(
                    200,
                    json!({ "device_auth_id": "dev-2", "user_code": "CODE-2", "interval": 1 }).to_string(),
                ),
            ],
        );
        server.route(
            "/auth/api/accounts/deviceauth/token",
            vec![
                Resp::status(307, "").header("location", &format!("{}/steal-device", attacker.base)),
                Resp::status(
                    200,
                    json!({ "authorization_code": "auth-code-secret", "code_verifier": "verifier-secret" }).to_string(),
                ),
            ],
        );
        server.route(
            "/auth/oauth/token",
            vec![Resp::status(308, "").header("location", &format!("{}/steal-code", attacker.base))],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-openai": {
                        "codex_base_url": format!("{base}/codex"),
                        "auth_base_url": format!("{base}/auth"),
                    }
                }
            }),
        )
        .await;

        let poll_log: String = eval(
            &h,
            r#"
            local logs, opened = {}, 0
            local orig_open = mare.proc.open
            mare.proc.open = function() opened = opened + 1 end
            pcall(LLM.login, LLM, "openai", {
              method = "device",
              say = function(s) logs[#logs + 1] = tostring(s) end,
              ask = function() return "" end,
            })
            mare.proc.open = orig_open
            assert(opened == 1, "expected browser fallback to invoke mare.proc.open")
            return table.concat(logs, "\n")
            "#,
        )
        .await;
        assert!(poll_log.contains("redirect"), "device polling must reject cross-origin 307: {poll_log}");

        let code_log: String = eval(
            &h,
            r#"
            local logs, opened = {}, 0
            local orig_open = mare.proc.open
            mare.proc.open = function() opened = opened + 1 end
            pcall(LLM.login, LLM, "openai", {
              method = "device",
              say = function(s) logs[#logs + 1] = tostring(s) end,
              ask = function() return "" end,
            })
            mare.proc.open = orig_open
            assert(opened == 1, "expected browser fallback to invoke mare.proc.open")
            return table.concat(logs, "\n")
            "#,
        )
        .await;
        assert!(code_log.contains("redirect"), "code exchange must reject cross-origin 308: {code_log}");
        let token_req = server
            .requests()
            .into_iter()
            .find(|r| r.path == "/auth/oauth/token")
            .expect("origin server must receive /auth/oauth/token");
        assert!(token_req.body.contains("auth-code-secret"));
        assert!(attacker.requests().is_empty(), "cross-origin target received requests: {:?}", attacker.requests());
        h.shutdown().await;
    })
    .await
}

/// `provider-openai` handles WebSocket via `provider/transport`, isolates pooled sockets by
/// `(session_id, provider, credential_id, proxy)`, and leaves transport handling on `provider/transport`.
#[tokio::test]
async fn websocket_provider_transport_isolates_proxy_and_session() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let (port, seen) = ws_server().await;
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-openai": {
                "base_url": format!("http://127.0.0.1:{port}/openai"), "transport": "websocket" } } }),
        )
        .await;
        expose(&h);
        let handled_via_bus: bool = eval(
            &h,
            r#"
local m = LLM:resolve("openai/gpt-6.1-terra")
local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
LLM:stream({ model = m, session_id = "sess-ws-1", messages = msgs, no_fallback = true }, function() end)
LLM:stream({ model = m, session_id = "sess-ws-2", messages = msgs, no_fallback = true }, function() end)

local resp = C.cordis:get("bus"):bail("provider/transport", {
  provider = "openai",
  transport = "websocket",
  session_id = "sess-ws-1",
  credential_id = "cred-1",
  proxy = "",
  request = {
    url = mare.config.plugins["provider-openai"].base_url .. "/responses",
    headers = { authorization = "Bearer test-openai" },
    body = { model = "gpt-6.1-terra", input = {} },
  },
})
local ok = type(resp) == "table" and resp.transport == "websocket"
if ok and resp.close then resp:close() end
return ok
"#,
        )
        .await;
        let g = seen.lock().unwrap().clone();
        assert_eq!(g.0, 3, "two distinct sessions + direct bus call must open distinct WebSocket connections");
        assert!(handled_via_bus, "provider-openai must handle WebSocket on provider/transport bus event");
        assert!(server.requests().is_empty(), "websocket mode must use WebSocket not HTTP");
        h.shutdown().await;
    })
    .await
}

/// Regression:
/// 1) Switching OAuth users in the same ChatGPT workspace (`account_id` unchanged) or rotating OAuth
///    `access` token must open a new WebSocket connection (never reuse a previous user's WS).
/// 2) HTTP 401 during WebSocket handshake must NOT permanently set `ws_off`, and must trigger
///    `on_unauthorized` token refresh + retry over WebSocket (`auto` transport).
#[tokio::test]
async fn openai_websocket_reconnects_on_oauth_account_switch_and_recovers_ws_401() {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::Message;

    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let (port, seen) = ws_server().await;
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-openai": {
                        "codex_base_url": format!("http://127.0.0.1:{port}"),
                        "transport": "websocket"
                    }
                }
            }),
        )
        .await;
        expose(&h);
        exec(
            &h,
            r#"
LLM:auth_set("openai-codex", {
  type = "oauth",
  access = "TOK-USER-1",
  refresh = "ref-1",
  expires = 9999999999999,
  account_id = "acct-shared",
  email = "user1@example.com"
})
"#,
        )
        .await;
        eval::<()>(
            &h,
            r#"
local m = LLM:resolve("openai/gpt-6.1-terra")
LLM:stream({ model = m, session_id = "same-sess", messages = { { role = "user", content = { { type = "text", text = "hi 1" } } } }, no_fallback = true }, function() end)
"#,
        )
        .await;
        exec(
            &h,
            r#"
LLM:auth_set("openai-codex", {
  type = "oauth",
  access = "TOK-USER-2",
  refresh = "ref-2",
  expires = 9999999999999,
  account_id = "acct-shared",
  email = "user2@example.com"
})
"#,
        )
        .await;
        eval::<()>(
            &h,
            r#"
local m = LLM:resolve("openai/gpt-6.1-terra")
LLM:stream({ model = m, session_id = "same-sess", messages = { { role = "user", content = { { type = "text", text = "hi 2" } } } }, no_fallback = true }, function() end)
"#,
        )
        .await;
        h.shutdown().await;
        let g = seen.lock().unwrap().clone();
        let conns = &g.3;
        assert_eq!(conns.len(), 2, "same account_id user switch must open a new WS: {conns:?}");
        assert_eq!(conns[0].0, "Bearer TOK-USER-1", "{conns:?}");
        assert_eq!(conns[1].0, "Bearer TOK-USER-2", "{conns:?}");
        assert_eq!(conns[0].1, 1);
        assert_eq!(conns[1].1, 1);
        assert!(server.requests().is_empty(), "nothing should hit HTTP fallback");
        drop(server);
        drop(env);

        // Part B: WS handshake HTTP 401 must run on_unauthorized refresh and retry via WS (not set ws_off).
        let mut env2 = TestEnv::new().await;
        let server2 = MockServer::start().await;
        server2.route(
            "/oauth/token",
            vec![Resp::status(
                200,
                json!({
                    "access_token": "TOK-FRESH-WS",
                    "refresh_token": "ref-fresh",
                    "expires_in": 3600
                })
                .to_string(),
            )],
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_port = listener.local_addr().unwrap().port();
        let auth_seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let auth_seen_srv = auth_seen.clone();
        let ws_srv = tokio::spawn(async move {
            for idx in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                if idx == 0 {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    let req_txt = String::from_utf8_lossy(&buf[..n]).to_string();
                    let auth_line = req_txt
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                        .unwrap_or("")
                        .to_string();
                    auth_seen_srv.lock().unwrap().push(auth_line);
                    let _ = stream
                        .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    continue;
                }
                let seen_c = auth_seen_srv.clone();
                #[allow(clippy::result_large_err)]
                let cb = move |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
                    let auth = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    seen_c.lock().unwrap().push(auth);
                    Ok(resp)
                };
                let mut ws = tokio_tungstenite::accept_hdr_async(stream, cb).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(_) = msg {
                        for frame in [
                            r#"{"type":"response.created","response":{"id":"resp_ws_401"}}"#,
                            r#"{"type":"response.output_item.added","item":{"type":"message"}}"#,
                            r#"{"type":"response.output_text.delta","delta":"ws-recovered"}"#,
                            r#"{"type":"response.completed","response":{"id":"resp_ws_401","status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}"#,
                        ] {
                            ws.send(Message::Text(frame.into())).await.unwrap();
                        }
                        if idx == 1 {
                            // Keep connection open so turn 2 can either reuse or connect fresh over WS.
                            continue;
                        }
                        break;
                    }
                }
            }
        });

        let h2 = provider_host(
            &mut env2,
            &server2,
            json!({
                "plugins": {
                    "provider-openai": {
                        "codex_base_url": format!("http://127.0.0.1:{ws_port}"),
                        "auth_base_url": server2.base,
                        "transport": "websocket"
                    }
                }
            }),
        )
        .await;
        expose(&h2);
        exec(
            &h2,
            r#"
LLM:auth_set("openai-codex", {
  type = "oauth",
  access = "TOK-STALE-WS",
  refresh = "ref-old",
  expires = 9999999999999,
  account_id = "acct-401",
  email = "401@example.com"
})
"#,
        )
        .await;
        let ok_res: bool = eval(
            &h2,
            r#"
local m = LLM:resolve("openai/gpt-6.1-terra")
local res1 = LLM:stream({ model = m, session_id = "s-401", messages = { { role = "user", content = { { type = "text", text = "hi 1" } } } }, no_fallback = true }, function() end)
local res2 = LLM:stream({ model = m, session_id = "s-401", messages = { { role = "user", content = { { type = "text", text = "hi 2" } } } }, no_fallback = true }, function() end)
return tostring(res1.content[1].text) == "ws-recovered" and tostring(res2.content[1].text) == "ws-recovered"
"#,
        )
        .await;
        assert!(ok_res, "WS 401 must recover via on_unauthorized and keep WS enabled for subsequent turns");
        let auths = auth_seen.lock().unwrap().clone();
        assert!(auths.len() >= 2, "expected initial 401 handshake + retry handshake: {auths:?}");
        assert!(auths[0].contains("TOK-STALE-WS"), "{auths:?}");
        assert_eq!(auths[1], "Bearer TOK-FRESH-WS", "{auths:?}");
        assert!(server2.requests().iter().all(|r| r.path == "/oauth/token"), "unexpected HTTP fallback requests: {:?}", server2.requests());
        ws_srv.abort();
        h2.shutdown().await;
    })
    .await
}

/// WebSocket pooling lifecycle & isolation:
/// 1) Reuses socket across consecutive turns, closes + redials when `fresh_connection = true` (`ev.fresh`),
///    and guards eviction by connection identity so closing an old response doesn't evict a replacement.
/// 2) `ws_off` and `ws_failures` are scoped per `(session_id, provider, credential_id, auth, proxy, url)`.
/// 3) Pooled sockets are explicitly closed (observing Close/EOF on server) on `session/changed`,
///    `llm/logout`, and plugin disposal (`reload` / `shutdown`).
#[tokio::test]
async fn websocket_fresh_connection_scoped_fallback_and_lifecycle_closure() {
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_tungstenite::tungstenite::Message;

    local(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connects = Arc::new(AtomicUsize::new(0));
        let frames_per_conn = Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
        let closed_count = Arc::new(AtomicUsize::new(0));

        let srv_connects = connects.clone();
        let srv_frames = frames_per_conn.clone();
        let srv_closed = closed_count.clone();
        let events: Vec<String> =
            fixture("openai-responses.sse").lines().filter_map(|l| l.strip_prefix("data: ").map(str::to_string)).collect();

        let srv_task = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let conn_idx = srv_connects.fetch_add(1, Ordering::SeqCst);
                srv_frames.lock().unwrap().push(0);
                let evs = events.clone();
                let frames_ref = srv_frames.clone();
                let closed_ref = srv_closed.clone();
                tokio::spawn(async move {
                    if let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await {
                        while let Some(msg) = ws.next().await {
                            match msg {
                                Ok(Message::Text(_)) => {
                                    frames_ref.lock().unwrap()[conn_idx] += 1;
                                    for ev in &evs {
                                        if ws.send(Message::Text(ev.clone().into())).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                                Ok(Message::Close(_)) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                    }
                    closed_ref.fetch_add(1, Ordering::SeqCst);
                });
            }
        });

        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/openai/responses",
            vec![Resp::sse(fixture("openai-responses.sse")), Resp::sse(fixture("openai-responses.sse"))],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-openai": {
                        "base_url": format!("http://127.0.0.1:{port}/openai"),
                        "transport": "websocket"
                    }
                }
            }),
        )
        .await;
        expose(&h);

        // 1. Two consecutive turns reuse socket #0 (1 connection, 2 frames);
        //    then `fresh_connection = true` closes socket #0 and dials socket #1.
        exec(
            &h,
            r#"
            local m = LLM:resolve("openai/gpt-6.1-terra")
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
            LLM:stream({ model = m, session_id = "ws-life-1", messages = msgs, no_fallback = true }, function() end)
            LLM:stream({ model = m, session_id = "ws-life-1", messages = msgs, no_fallback = true }, function() end)
            LLM:stream({ model = m, session_id = "ws-life-1", fresh_connection = true, messages = msgs, no_fallback = true }, function() end)
            "#,
        )
        .await;
        wait_until(5, "old connection closed on fresh_connection", || {
            let c = closed_count.clone();
            async move { c.load(Ordering::SeqCst) >= 1 }
        })
        .await;
        assert_eq!(connects.load(Ordering::SeqCst), 2, "fresh_connection must open a new socket");
        assert_eq!(*frames_per_conn.lock().unwrap(), vec![2, 1], "turns 1+2 reuse conn 0, fresh uses conn 1");

        // 2. Identity-guarded eviction: old response close() must NOT evict newer replacement connection.
        let identity_ok: bool = eval(
            &h,
            r#"
            local bus = C.cordis:get("bus")
            local wire = {
              url = mare.config.plugins["provider-openai"].base_url .. "/responses",
              headers = { authorization = "Bearer test-openai" },
              body = { model = "gpt-6.1-terra", input = {} },
            }
            local r_old = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "ws-guard",
              proxy = "", request = wire,
            })
            local r_new = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "ws-guard",
              proxy = "", fresh = true, request = wire,
            })
            -- Finish r_new's frames first so socket stays pooled, then close r_old afterwards.
            while r_new:next_event() do end
            r_old:close()
            -- Next turn without fresh MUST reuse r_new's socket (connects does not grow again).
            local r_reused = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "ws-guard",
              proxy = "", request = wire,
            })
            while r_reused:next_event() do end
            return r_reused.transport == "websocket"
            "#,
        )
        .await;
        assert!(identity_ok);
        // Only r_old (#2) + r_new (#3) were opened, NOT a 3rd one for r_reused.
        assert_eq!(connects.load(Ordering::SeqCst), 4, "old response close must not evict r_new");

        // 3a. Socket closure on `session/changed`.
        let closed_before_session_switch = closed_count.load(Ordering::SeqCst);
        exec(
            &h,
            r#"C.cordis:get("bus"):emit("session/changed", { id = "ws-other-session", previous_id = "ws-life-1", reason = "new" })"#,
        )
        .await;
        wait_until(5, "socket closed on session/changed", || {
            let c = closed_count.clone();
            async move { c.load(Ordering::SeqCst) > closed_before_session_switch }
        })
        .await;

        // 3b. Socket closure on `llm/logout`.
        let closed_before_logout = closed_count.load(Ordering::SeqCst);
        exec(&h, r#"C.cordis:get("bus"):emit("llm/logout", { provider = "openai", removed = { "openai" } })"#).await;
        wait_until(5, "socket closed on llm/logout", || {
            let c = closed_count.clone();
            async move { c.load(Ordering::SeqCst) > closed_before_logout }
        })
        .await;

        // 3c. Scoped fallback isolation: handshake/connect failure in session-bad must not disable WS in session-good.
        // Force `transport = "websocket"` so `ws_response` runs against a counting dead listener:
        // Call #1 increments `dead_accepts` to 1 and sets `ws_off[sess-bad]`; Call #2 must NOT redial (`dead_accepts` stays 1).
        {
            let dead_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let dead_port = dead_listener.local_addr().unwrap().port();
            let dead_accepts = Arc::new(AtomicUsize::new(0));
            let dead_c = dead_accepts.clone();
            let dead_srv = tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                while let Ok((mut sock, _)) = dead_listener.accept().await {
                    dead_c.fetch_add(1, Ordering::SeqCst);
                    let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n").await;
                    let _ = sock.shutdown().await;
                }
            });
            global(&h, "DEAD_WS_URL", format!("http://127.0.0.1:{dead_port}/responses"));
            let scoped_res: bool = eval(
                &h,
                r#"
                local bus = C.cordis:get("bus")
                local dead = bus:bail("provider/transport", {
                  provider = "openai", transport = "websocket", session_id = "sess-bad", proxy = "",
                  request = { url = DEAD_WS_URL, headers = { authorization = "Bearer k1" }, body = {} },
                })
                assert(dead == nil, "dead port falls back to HTTP")
                local dead_again = bus:bail("provider/transport", {
                  provider = "openai", transport = "websocket", session_id = "sess-bad", proxy = "",
                  request = { url = DEAD_WS_URL, headers = { authorization = "Bearer k1" }, body = {} },
                })
                assert(dead_again == nil, "ws_off keeps sess-bad disabled")

                -- Distinct case-normalized Authorization header (`Authorization` vs `AUTHORIZATION`) creates a distinct scope key!
                local r_auth1 = bus:bail("provider/transport", {
                  provider = "openai", transport = "websocket", session_id = "sess-auth-ci", proxy = "",
                  request = {
                    url = mare.config.plugins["provider-openai"].base_url .. "/responses",
                    headers = { Authorization = "Bearer token-one" },
                    body = { model = "gpt-6.1-terra", input = {} },
                  },
                })
                while r_auth1:next_event() do end
                local r_auth2 = bus:bail("provider/transport", {
                  provider = "openai", transport = "websocket", session_id = "sess-auth-ci", proxy = "",
                  request = {
                    url = mare.config.plugins["provider-openai"].base_url .. "/responses",
                    headers = { AUTHORIZATION = "Bearer token-two" },
                    body = { model = "gpt-6.1-terra", input = {} },
                  },
                })
                while r_auth2:next_event() do end

                local ok = bus:bail("provider/transport", {
                  provider = "openai", transport = "websocket", session_id = "sess-good", proxy = "",
                  request = {
                    url = mare.config.plugins["provider-openai"].base_url .. "/responses",
                    headers = { authorization = "Bearer test-openai" },
                    body = { model = "gpt-6.1-terra", input = {} },
                  },
                })
                assert(ok ~= nil and ok.transport == "websocket", "sess-good must remain eligible for WS")
                while ok:next_event() do end
                return true
                "#,
            )
            .await;
            assert!(scoped_res);
            assert_eq!(
                dead_accepts.load(Ordering::SeqCst),
                1,
                "second call for failed scope must skip redialing via ws_off"
            );
            dead_srv.abort();
        }
        // r_auth1 (#4), r_auth2 (#5 - distinct auth header despite mixed case!), sess-good (#6)
        assert_eq!(connects.load(Ordering::SeqCst), 7, "distinct case-insensitive Authorization tokens must isolate pool keys");

        // 3e. Overlapping requests on the same scope lease pooled sockets through terminal frames
        //     so concurrent reads never steal frames from each other.
        let overlap_before = connects.load(Ordering::SeqCst);
        let overlap_ok: bool = eval(
            &h,
            r#"
            local bus = C.cordis:get("bus")
            local wire = {
              url = mare.config.plugins["provider-openai"].base_url .. "/responses",
              headers = { Authorization = "Bearer overlap-tok" },
              body = { model = "gpt-6.1-terra", input = {} },
            }
            local r1 = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "sess-overlap", proxy = "", request = wire,
            })
            local r2 = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "sess-overlap", proxy = "", request = wire,
            })
            assert(r1 and r2, "both overlapping WS requests must succeed")
            -- Interleave next_event reads across both responses: both must complete cleanly.
            local c1, c2 = 0, 0
            while true do
              local e1 = r1:next_event()
              local e2 = r2:next_event()
              if e1 then c1 = c1 + 1 end
              if e2 then c2 = c2 + 1 end
              if not e1 and not e2 then break end
            end
            return c1 > 0 and c1 == c2
            "#,
        )
        .await;
        assert!(overlap_ok, "overlapping WS streams must not interleave or drop frames");
        assert_eq!(connects.load(Ordering::SeqCst), overlap_before + 2, "overlapping request must open a separate socket");

        // 3f. In-flight dial retired by `llm/logout` closes cleanly when handshake completes without leaking into pool or ws_off.
        {
            let stall_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let stall_port = stall_listener.local_addr().unwrap().port();
            let accepted = Arc::new(AtomicUsize::new(0));
            let stalled_closed = Arc::new(AtomicUsize::new(0));
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            let acc_c = accepted.clone();
            let sc_c = stalled_closed.clone();
            let stall_task = tokio::spawn(async move {
                if let Ok((sock, _)) = stall_listener.accept().await {
                    acc_c.fetch_add(1, Ordering::SeqCst);
                    let _ = release_rx.await;
                    if let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await {
                        while ws.next().await.is_some() {}
                    }
                    sc_c.fetch_add(1, Ordering::SeqCst);
                }
            });
            global(&h, "STALL_URL", format!("http://127.0.0.1:{stall_port}/responses"));
            exec(
                &h,
                r#"
                INFLIGHT_DIAL_RES = nil
                mare.task.spawn(function()
                  INFLIGHT_DIAL_RES = C.cordis:get("bus"):bail("provider/transport", {
                    provider = "openai", transport = "websocket", session_id = "sess-stall", proxy = "",
                    request = { url = STALL_URL, headers = { authorization = "Bearer stall" }, body = { model = "gpt-6.1-terra", input = {} } },
                  })
                end)
                "#,
            )
            .await;
            wait_until(5, "stalled WS listener accepted connection", || {
                let a = accepted.clone();
                async move { a.load(Ordering::SeqCst) >= 1 }
            })
            .await;
            // Retire scope via llm/logout BEFORE handshake completes.
            exec(&h, r#"C.cordis:get("bus"):emit("llm/logout", { provider = "openai" })"#).await;
            let _ = release_tx.send(());
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), stall_task).await;
            wait_until(5, "retired in-flight dial closed immediately", || {
                let s = stalled_closed.clone();
                async move { s.load(Ordering::SeqCst) >= 1 }
            })
            .await;
            let dial_aborted: bool = eval(
                &h,
                "return type(INFLIGHT_DIAL_RES) == 'table' and INFLIGHT_DIAL_RES.aborted == true",
            )
            .await;
            assert!(dial_aborted, "retired in-flight dial must return aborted marker instead of pooling or falling back");
        }

        // 3g. Real `LLM:stream` in-flight WS dial retired by `llm/logout` aborts cleanly WITHOUT HTTP fallback!
        {
            let stall_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let stall_port = stall_listener.local_addr().unwrap().port();
            let accepted = Arc::new(AtomicUsize::new(0));
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
            let acc_c = accepted.clone();
            let stall_srv = tokio::spawn(async move {
                let mut rx = Some(release_rx);
                loop {
                    tokio::select! {
                        _ = &mut stop_rx => break,
                        res = stall_listener.accept() => {
                            let Ok((sock, _)) = res else { break };
                            let count = acc_c.fetch_add(1, Ordering::SeqCst);
                            if count == 0 {
                                if let Some(r) = rx.take() {
                                    let _ = r.await;
                                }
                                if let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await {
                                    while ws.next().await.is_some() {}
                                }
                            }
                        }
                    }
                }
            });
            let h2 = provider_host(
                &mut env,
                &server,
                json!({
                    "plugins": {
                        "provider-openai": {
                            "base_url": format!("http://127.0.0.1:{stall_port}/openai"),
                            "transport": "websocket"
                        }
                    }
                }),
            )
            .await;
            expose(&h2);
            exec(
                &h2,
                r#"
                STREAM_DONE = false
                STREAM_RES = nil
                mare.task.spawn(function()
                  local m = LLM:resolve("openai/gpt-6.1-terra")
                  local ok, res = pcall(LLM.stream, LLM, {
                    model = m,
                    session_id = "sess-stream-abort",
                    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
                    no_fallback = true,
                  }, function() end)
                  STREAM_RES = { ok = ok, res = res }
                  STREAM_DONE = true
                end)
                "#,
            )
            .await;
            wait_until(5, "stalled LLM:stream WS listener accepted connection", || {
                let a = accepted.clone();
                async move { a.load(Ordering::SeqCst) >= 1 }
            })
            .await;
            exec(&h2, r#"C.cordis:get("bus"):emit("llm/logout", { provider = "openai" })"#).await;
            let _ = release_tx.send(());
            wait_until(5, "stalled LLM:stream finished as aborted", || {
                let h2c = h2.clone();
                async move { eval::<bool>(&h2c, "return STREAM_DONE == true").await }
            })
            .await;
            let _ = stop_tx.send(());
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), stall_srv).await;
            let (ok_flag, stop_reason, err_kind): (bool, String, String) = eval(
                &h2,
                r#"
                return STREAM_RES.ok == true,
                  tostring(type(STREAM_RES.res) == 'table' and STREAM_RES.res.stop_reason or ''),
                  tostring(type(STREAM_RES.res) == 'table' and STREAM_RES.res.error_kind or '')
                "#,
            )
            .await;
            assert!(
                ok_flag && stop_reason == "aborted",
                "retired WS dial in LLM:stream must return aborted terminal message, got ok={ok_flag} stop={stop_reason} kind={err_kind}"
            );
            assert_eq!(
                accepted.load(Ordering::SeqCst),
                1,
                "retired WS dial must NEVER open an HTTP fallback connection"
            );
            h2.shutdown().await;
        }

        // 3h. `fresh = true` while an overlapping request is `busy` opens a fresh socket while
        //     letting the busy stream finish reading its terminal frame cleanly!
        let fresh_busy_before = connects.load(Ordering::SeqCst);
        let fresh_busy_ok: bool = eval(
            &h,
            r#"
            local bus = C.cordis:get("bus")
            local wire = {
              url = mare.config.plugins["provider-openai"].base_url .. "/responses",
              headers = { Authorization = "Bearer busy-tok", ["x-api-key"] = "key-A" },
              body = { model = "gpt-6.1-terra", input = {} },
            }
            local r1 = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "sess-fresh-busy", proxy = "", request = wire,
            })
            assert(r1 ~= nil, "first busy stream must open")
            local first_ev = r1:next_event()
            assert(first_ev ~= nil, "r1 read initial frame while busy")
            local r2 = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "sess-fresh-busy", proxy = "", fresh = true, request = wire,
            })
            assert(r2 ~= nil, "fresh stream must open a new socket")
            local c2 = 0
            while r2:next_event() do c2 = c2 + 1 end
            local c1 = 1
            while r1:next_event() do c1 = c1 + 1 end
            -- Multi-header x-api-key difference creates a distinct scope key:
            local r3 = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "sess-fresh-busy", proxy = "",
              request = {
                url = wire.url,
                headers = { Authorization = "Bearer busy-tok", ["x-api-key"] = "key-B" },
                body = wire.body,
              },
            })
            while r3:next_event() do end
            return c1 > 1 and c2 > 1
            "#,
        )
        .await;
        assert!(fresh_busy_ok, "fresh=true must NOT reset active busy socket");
        assert_eq!(
            connects.load(Ordering::SeqCst),
            fresh_busy_before + 3,
            "busy r1, fresh r2, and distinct x-api-key r3 must each dial a socket"
        );

        // 3i. Consecutive mid-stream failures (`ws_failures >= 2`) fall back to HTTP on the 3rd request.
        {
            use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
            let drop_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let drop_port = drop_listener.local_addr().unwrap().port();
            let http_fallbacks = Arc::new(AtomicUsize::new(0));
            let http_fb_c = http_fallbacks.clone();
            let sse_body = fixture("openai-responses.sse");
            let drop_srv = tokio::spawn(async move {
                // Attempts 1 & 2: WS handshake succeeds, then close abruptly mid-stream before terminal frame.
                for _ in 0..2 {
                    if let Ok((sock, _)) = drop_listener.accept().await
                        && let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await
                    {
                        let _ = ws.next().await;
                        let _ = ws.close(None).await;
                    }
                }
                // Attempt 3: HTTP POST fallback on the same endpoint URL.
                if let Ok((sock, _)) = drop_listener.accept().await {
                    let mut reader = tokio::io::BufReader::new(sock);
                    let mut content_len = 0usize;
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some((k, v)) = line.split_once(':')
                            && k.trim().eq_ignore_ascii_case("content-length")
                        {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    if content_len > 0 {
                        let mut body = vec![0u8; content_len];
                        let _ = reader.read_exact(&mut body).await;
                    }
                    let mut sock = reader.into_inner();
                    http_fb_c.fetch_add(1, Ordering::SeqCst);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        sse_body.len(),
                        sse_body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                }
            });
            let h3 = provider_host(
                &mut env,
                &server,
                json!({
                    "plugins": {
                        "llm": { "retries": 2, "retry_base_ms": 1, "retry_max_ms": 2 },
                        "provider-openai": {
                            "base_url": format!("http://127.0.0.1:{drop_port}/openai"),
                            "transport": "websocket"
                        }
                    }
                }),
            )
            .await;
            expose(&h3);
            let msg_json: String = eval(
                &h3,
                r#"
                local m = LLM:resolve("openai/gpt-6.1-terra")
                local res = LLM:stream({
                  model = m,
                  session_id = "sess-drop-2x",
                  messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
                  no_fallback = true,
                }, function() end)
                return mare.json.encode(res)
                "#,
            )
            .await;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), drop_srv).await;
            let msg: Value = serde_json::from_str(&msg_json).unwrap();
            assert_eq!(msg["stop_reason"], "tool_use", "{msg}");
            assert_eq!(
                http_fallbacks.load(Ordering::SeqCst),
                1,
                "3rd attempt after 2 mid-stream WS drops must fall back to HTTP: {msg}"
            );
            h3.shutdown().await;
        }

        // 3j. Arbitrary custom header (`X-Workspace`, `Cookie`) isolation + case-insensitive reuse,
        //     and retired active stream errors on `llm/logout` MUST NOT increment `ws_failures`.
        let hdr_before = connects.load(Ordering::SeqCst);
        let hdr_ok: bool = eval(
            &h,
            r#"
            local bus = C.cordis:get("bus")
            local url = mare.config.plugins["provider-openai"].base_url .. "/responses"
            local body = { model = "gpt-6.1-terra", input = {} }

            -- Matching headers differing only in case share one pooled connection.
            local r1 = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "sess-hdrs", proxy = "",
              request = { url = url, headers = { Authorization = "Bearer h-tok", ["X-Workspace"] = "ws-a", Cookie = "c=1" }, body = body },
            })
            while r1:next_event() do end
            local r2 = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "sess-hdrs", proxy = "",
              request = { url = url, headers = { authorization = "Bearer h-tok", ["x-workspace"] = "ws-a", cookie = "c=1" }, body = body },
            })
            while r2:next_event() do end

            -- Distinct X-Workspace and distinct Cookie each isolate into separate connections.
            local r3 = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "sess-hdrs", proxy = "",
              request = { url = url, headers = { authorization = "Bearer h-tok", ["X-Workspace"] = "ws-b", Cookie = "c=1" }, body = body },
            })
            while r3:next_event() do end
            local r4 = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "sess-hdrs", proxy = "",
              request = { url = url, headers = { authorization = "Bearer h-tok", ["X-Workspace"] = "ws-a", Cookie = "c=2" }, body = body },
            })
            while r4:next_event() do end

            -- Duplicate case-variant headers in one table resolve via last-string-wins matching ws.rs.
            local dupe_hdrs = { Authorization = "Bearer h-dupe", ["X-Workspace"] = "ws-dupe-a", ["x-workspace"] = "ws-dupe-b" }
            local expected_ws
            for k, v in pairs(dupe_hdrs) do
              if k:lower() == "x-workspace" then expected_ws = v end
            end
            local rd1 = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "sess-hdrs-dupe", proxy = "",
              request = { url = url, headers = dupe_hdrs, body = body },
            })
            while rd1:next_event() do end
            local rd2 = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "sess-hdrs-dupe", proxy = "",
              request = { url = url, headers = { authorization = "Bearer h-dupe", ["x-workspace"] = expected_ws }, body = body },
            })
            while rd2:next_event() do end

            -- Two active streams retired by `llm/logout` while mid-stream must NOT poison `ws_failures` (>= 2)!
            local wire_retire = { url = url, headers = { Authorization = "Bearer retire-fail-tok" }, body = body }
            for _ = 1, 2 do
              local ra = bus:bail("provider/transport", {
                provider = "openai", transport = "websocket", session_id = "sess-retire-err", proxy = "", request = wire_retire,
              })
              assert(ra ~= nil and ra:next_event() ~= nil, "stream must start")
              bus:emit("llm/logout", { provider = "openai", removed = { "openai" } })
              pcall(function() while ra:next_event() do end end)
            end
            local after_retires = bus:bail("provider/transport", {
              provider = "openai", transport = "websocket", session_id = "sess-retire-err", proxy = "", request = wire_retire,
            })
            assert(after_retires ~= nil and after_retires.transport == "websocket",
              "retired active stream close/error must NOT accumulate ws_failures")
            while after_retires:next_event() do end
            return true
            "#,
        )
        .await;
        assert!(hdr_ok);
        assert_eq!(
            connects.load(Ordering::SeqCst),
            hdr_before + 7,
            "r1/r2 reuse (1), r3 X-Workspace (2), r4 Cookie (3), rd1/rd2 dupe reuse (4), plus 3 retire-err connects = 7"
        );

        // 3d. Plugin disposal (`ctx:effect`) closes remaining pooled sockets on shutdown.
        let total_opened = connects.load(Ordering::SeqCst);
        h.shutdown().await;
        wait_until(5, "all remaining sockets closed on plugin disposal", || {
            let c = closed_count.clone();
            async move { c.load(Ordering::SeqCst) >= total_opened }
        })
        .await;
        srv_task.abort();
    })
    .await
}

/// A single transient WebSocket connect failure must not permanently degrade the whole session to
/// HTTP; the next request on the same scope retries WebSocket. After repeated transient failures,
/// temporary fallback expires after the cooldown and recovers to WebSocket.
#[tokio::test]
async fn openai_websocket_recovers_after_transient_connect_failure_and_cooldown() {
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_tungstenite::tungstenite::Message;
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        let server = MockServer::start().await;

        // Grab an ephemeral port and drop it so initial dials hit connection refused (transient).
        let probe_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe_listener.local_addr().unwrap().port();
        drop(probe_listener);

        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-openai": {
                "base_url": format!("http://127.0.0.1:{port}/openai"),
                "transport": "websocket",
            } } }),
        )
        .await;
        expose(&h);

        global(&h, "WS_URL", format!("http://127.0.0.1:{port}/openai/responses"));

        // Call #1: transient connect failure (port closed) -> falls back to HTTP for this request only.
        let first_fell_back: bool = eval(
            &h,
            r#"local bus = C.cordis:get("bus")
               local r = bus:bail("provider/transport", {
                 provider = "openai", transport = "websocket", session_id = "sess-recover", proxy = "",
                 request = { url = WS_URL, headers = { authorization = "Bearer k1" }, body = { model = "gpt-6.1-terra", input = {} } },
               })
               return r == nil"#,
        )
        .await;
        assert!(first_fell_back, "first transient connect failure falls back to HTTP");

        // Now bring a real WebSocket server up on the exact same port.
        let ws_listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let connects = Arc::new(AtomicUsize::new(0));
        let c = connects.clone();
        let ws_srv = tokio::spawn(async move {
            while let Ok((sock, _)) = ws_listener.accept().await {
                let c = c.clone();
                tokio::spawn(async move {
                    if let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await {
                        c.fetch_add(1, Ordering::SeqCst);
                        while let Some(Ok(Message::Text(_))) = ws.next().await {
                            let _ = ws.send(Message::Text(r#"{"type":"response.output_text.delta","delta":"ok"}"#.into())).await;
                            let _ = ws.send(Message::Text(r#"{"type":"response.completed","response":{"id":"r1","status":"completed"}}"#.into())).await;
                        }
                    }
                });
            }
        });

        // Call #2 on the SAME session/scope must retry and succeed over WebSocket!
        let recovered: bool = eval(
            &h,
            r#"local bus = C.cordis:get("bus")
               local r = bus:bail("provider/transport", {
                 provider = "openai", transport = "websocket", session_id = "sess-recover", proxy = "",
                 request = { url = WS_URL, headers = { authorization = "Bearer k1" }, body = { model = "gpt-6.1-terra", input = {} } },
               })
               if not r or r.transport ~= "websocket" then return false end
               while r:next_event() do end
               return true"#,
        )
        .await;
        assert!(recovered, "single transient connect failure must not permanently disable WebSocket for the session");
        assert_eq!(connects.load(Ordering::SeqCst), 1);

        // Part B: Two consecutive transient failures engage cooldown, then recover once cooldown expires.
        let p2 = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        global(&h, "WS_URL2", format!("http://127.0.0.1:{p2}/openai/responses"));
        let cooldown_started: bool = eval(
            &h,
            r#"local ws_wire = require("provider-openai.wire")
               ws_wire.WS_FALLBACK_COOLDOWN_MS = 60
               local bus = C.cordis:get("bus")
               local req = {
                 provider = "openai", transport = "websocket", session_id = "sess-cooldown", proxy = "",
                 request = { url = WS_URL2, headers = { authorization = "Bearer k2" }, body = { model = "gpt-6.1-terra", input = {} } },
               }
               assert(bus:bail("provider/transport", req) == nil)
               assert(bus:bail("provider/transport", req) == nil)
               return true"#,
        )
        .await;
        assert!(cooldown_started);

        let ws_listener2 = tokio::net::TcpListener::bind(("127.0.0.1", p2)).await.unwrap();
        let connects2 = Arc::new(AtomicUsize::new(0));
        let c2 = connects2.clone();
        let ws_srv2 = tokio::spawn(async move {
            while let Ok((sock, _)) = ws_listener2.accept().await {
                let c2 = c2.clone();
                tokio::spawn(async move {
                    if let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await {
                        c2.fetch_add(1, Ordering::SeqCst);
                        while let Some(Ok(Message::Text(_))) = ws.next().await {
                            let _ = ws.send(Message::Text(r#"{"type":"response.completed","response":{"id":"r2","status":"completed"}}"#.into())).await;
                        }
                    }
                });
            }
        });

        let (skipped_during_cooldown, recovered_after_cooldown): (bool, bool) = eval(
            &h,
            r#"local bus = C.cordis:get("bus")
               local req = {
                 provider = "openai", transport = "websocket", session_id = "sess-cooldown", proxy = "",
                 request = { url = WS_URL2, headers = { authorization = "Bearer k2" }, body = { model = "gpt-6.1-terra", input = {} } },
               }
               local during = bus:bail("provider/transport", req)
               mare.task.sleep(90)
               local after = bus:bail("provider/transport", req)
               if after then while after:next_event() do end end
               return during == nil, after ~= nil and after.transport == "websocket""#,
        )
        .await;
        assert!(skipped_during_cooldown, "while cooldown is active, requests should skip WebSocket");
        assert!(recovered_after_cooldown, "after cooldown expires, same scope should recover to WebSocket");
        assert_eq!(connects2.load(Ordering::SeqCst), 1);

        ws_srv.abort();
        ws_srv2.abort();
        h.shutdown().await;
    })
    .await
}
