//! A stream ends at its terminal event, not at the connection's EOF (provider corpus;
//! reference implementation its incomplete variant :379).
//! A server or proxy may keep the response open after the last event; waiting for EOF then
//! turns a finished answer into an idle-watchdog failure and a second, billed request.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

/// An SSE server that writes `body`, keeps the TCP connection open (no EOF), and notifies
/// `disconnected` as soon as the client closes/drops its end of the socket.
async fn keep_open_sse_server(body: String) -> (String, Arc<AtomicUsize>, Arc<Notify>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let requests = Arc::new(AtomicUsize::new(0));
    let disconnected = Arc::new(Notify::new());
    let (req_c, disc_c) = (requests.clone(), disconnected.clone());
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let body = body.clone();
            let req_c = req_c.clone();
            let disc_c = disc_c.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let (head_end, len) = loop {
                    let Ok(n) = sock.read(&mut tmp).await else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let Ok(n) = sock.read(&mut tmp).await else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                req_c.fetch_add(1, Ordering::SeqCst);
                let out =
                    format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}");
                if sock.write_all(out.as_bytes()).await.is_err() || sock.flush().await.is_err() {
                    return;
                }
                // Keep the connection open until the client disconnects (EOF/reset on server read).
                while let Ok(n) = sock.read(&mut tmp).await {
                    if n == 0 {
                        break;
                    }
                }
                disc_c.notify_one();
            });
        }
    });
    (base, requests, disconnected)
}

async fn try_stream(h: &Host, model: &str) -> Result<Value, String> {
    let code = format!(
        r#"
local model = ...
local ok, res = pcall(function(...)
  {STREAM}
end, model)
if ok and res.stop_reason ~= "error" then return res end
if ok then error(res.error) end
error(type(res) == "table" and (res.message or mare.json.encode(res)) or tostring(res))
"#
    );
    let f = h.lua().load(&code).into_function().unwrap();
    match f.call_async::<mlua::Value>(model).await {
        Ok(v) => Ok(mare_shared::lua::lua_to_json(h.lua(), v).unwrap()),
        Err(e) => Err(e.to_string()),
    }
}

fn host_extra_for(wire: &str, base: &str) -> Value {
    match wire {
        "responses" => {
            json!({ "plugins": { "provider-openai": { "base_url": format!("{base}/openai"), "transport": "http" } } })
        }
        "anthropic" => json!({ "plugins": { "provider-anthropic": { "base_url": format!("{base}/anthropic") } } }),
        "compat" => {
            json!({ "providers": { "deepseek": { "base_url": format!("{base}/compat") } } })
        }
        other => panic!("unknown wire {other}"),
    }
}

/// Each wire with an explicit terminator — Responses `response.completed`, Anthropic
/// `message_stop`, Chat Completions `[DONE]` — gets its complete fixture stream, after which the server keeps the connection
/// open (no EOF) far beyond the test's bound. The message must come back complete from that one
/// request, without a retry, and the client must close the connection before host shutdown.
#[tokio::test]
async fn a_complete_stream_ends_at_its_terminal_event_while_the_connection_stays_open() {
    let mut hung = Vec::new();
    for (wire, file, model, cache_write) in [
        ("responses", "openai-responses.sse", "openai/gpt-6.1-terra", 0),
        ("anthropic", "anthropic.sse", "anthropic/claude-sonnet-5-5", 5),
        ("compat", "openai-compat.sse", "deepseek/deepseek-reasoner", 0),
    ] {
        local(async {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let (base, requests, disconnected) = keep_open_sse_server(fixture(file)).await;
            let h = provider_host(&mut env, &server, host_extra_for(wire, &base)).await;
            match tokio::time::timeout(Duration::from_secs(5), stream(&h, model)).await {
                Ok(msg) => {
                    assert_fixture_message(wire, &msg, cache_write);
                    assert_eq!(requests.load(Ordering::SeqCst), 1, "{wire}: the finished stream was requested again");
                    tokio::time::timeout(Duration::from_secs(5), disconnected.notified())
                        .await
                        .unwrap_or_else(|_| panic!("{wire}: client did not close connection after terminal event"));
                }
                Err(_) => hung.push(wire),
            }
            h.shutdown().await;
        })
        .await;
    }
    assert!(hung.is_empty(), "the message did not end at its terminal event (still waiting for EOF) on: {hung:?}");
}

/// `response.incomplete` (max_output_tokens) must also finish and close the connection without waiting for EOF.
#[tokio::test]
async fn response_incomplete_ends_without_eof_and_closes_connection() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_inc\",\"status\":\"in_progress\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"partial\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"partial\",\"annotations\":[]}]}}\n\n",
            "event: response.incomplete\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_inc\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
        )
        .to_string();
        let (base, requests, disconnected) = keep_open_sse_server(body).await;
        let h = provider_host(&mut env, &server, host_extra_for("responses", &base)).await;
        let msg = tokio::time::timeout(Duration::from_secs(5), stream(&h, "openai/gpt-6.1-terra"))
            .await
            .expect("response.incomplete hung waiting for EOF");
        assert_eq!(msg["stop_reason"], "length");
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        tokio::time::timeout(Duration::from_secs(5), disconnected.notified())
            .await
            .expect("client did not close connection after response.incomplete");
        h.shutdown().await;
    })
    .await;
}

/// `response.failed` and in-stream `error` events across Responses, Anthropic, and Chat must end
/// immediately and close the connection without waiting for EOF.
#[tokio::test]
async fn terminal_error_events_end_without_eof_and_close_connection() {
    let cases = [
        (
            "responses-failed",
            "responses",
            "openai/gpt-6.1-terra",
            concat!(
                "event: response.created\n",
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_f\",\"status\":\"in_progress\"}}\n\n",
                "event: response.failed\n",
                "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"invalid_request_error\",\"message\":\"bad prompt\"}}}\n\n",
            ),
            "bad prompt",
        ),
        (
            "responses-error",
            "responses",
            "openai/gpt-6.1-terra",
            "event: error\ndata: {\"type\":\"error\",\"code\":\"invalid_request_error\",\"message\":\"stream error responses\"}\n\n",
            "stream error responses",
        ),
        (
            "anthropic-error",
            "anthropic",
            "anthropic/claude-sonnet-5-5",
            concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_e\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                "event: error\n",
                "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"stream error anthropic\"}}\n\n",
            ),
            "stream error anthropic",
        ),
        (
            "compat-error",
            "compat",
            "deepseek/deepseek-reasoner",
            "data: {\"error\":{\"code\":\"invalid_request_error\",\"message\":\"stream error compat\"}}\n\n",
            "stream error compat",
        ),
    ];

    for (label, wire, model, body, expected_err) in cases {
        local(async {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let (base, requests, disconnected) = keep_open_sse_server(body.to_string()).await;
            let h = provider_host(&mut env, &server, host_extra_for(wire, &base)).await;
            let err = tokio::time::timeout(Duration::from_secs(5), try_stream(&h, model))
                .await
                .unwrap_or_else(|_| panic!("{label}: hung waiting for EOF"))
                .unwrap_err();
            assert!(err.contains(expected_err), "{label}: expected {expected_err:?} in {err:?}");
            assert_eq!(requests.load(Ordering::SeqCst), 1, "{label}: should not retry non-retryable error");
            tokio::time::timeout(Duration::from_secs(5), disconnected.notified())
                .await
                .unwrap_or_else(|_| panic!("{label}: client did not close connection after terminal error"));
            h.shutdown().await;
        })
        .await;
    }
}

#[tokio::test]
async fn response_done_events_are_normalized_preserving_status() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let done_ok = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"DONE-OK\"}]}}\n\n",
            "event: response.done\n",
            "data: {\"type\":\"response.done\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
        );
        let done_incomplete = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"m2\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"DONE-LEN\"}]}}\n\n",
            "event: response.done\n",
            "data: {\"type\":\"response.done\",\"response\":{\"id\":\"resp_2\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
        );
        let done_failed = concat!(
            "event: response.done\n",
            "data: {\"type\":\"response.done\",\"response\":{\"id\":\"resp_3\",\"status\":\"failed\",\"error\":{\"code\":\"server_error\",\"message\":\"backend done fail\"}}}\n\n",
        );
        let (base, _requests, _disconnected) = keep_open_sse_server(done_ok.to_string()).await;
        let h = provider_host(&mut env, &server, host_extra_for("responses", &base)).await;
        let msg: serde_json::Value = tokio::time::timeout(
            Duration::from_secs(5),
            try_stream(&h, "openai/gpt-6.1-terra"),
        )
        .await
        .expect("response.done completed must end stream cleanly")
        .unwrap();
        assert_eq!(msg["stop_reason"], "stop");
        assert_eq!(msg["content"][0]["text"], "DONE-OK");
        h.shutdown().await;

        let (base2, _r2, _d2) = keep_open_sse_server(done_incomplete.to_string()).await;
        let h2 = provider_host(&mut env, &server, host_extra_for("responses", &base2)).await;
        let msg2: serde_json::Value = tokio::time::timeout(
            Duration::from_secs(5),
            try_stream(&h2, "openai/gpt-6.1-terra"),
        )
        .await
        .expect("response.done incomplete must end stream")
        .unwrap();
        assert_eq!(msg2["stop_reason"], "length");
        h2.shutdown().await;

        let (base3, _r3, _d3) = keep_open_sse_server(done_failed.to_string()).await;
        let h3 = provider_host(&mut env, &server, host_extra_for("responses", &base3)).await;
        let err3 = tokio::time::timeout(Duration::from_secs(5), try_stream(&h3, "openai/gpt-6.1-terra"))
            .await
            .expect("response.done failed must end stream")
            .unwrap_err();
        assert!(err3.contains("backend done fail"), "{err3}");
        h3.shutdown().await;
    })
    .await
}
