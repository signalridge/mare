//! Public opencode recordings, replayed as cassettes (cassette v1, unchanged) through the real
//! provider plugins: `tests/fixtures/cassettes/opencode/` (anomalyco/opencode@0e447fc,
//! `packages/ai/test/fixtures/recordings/`, MIT; provenance and selection in its SOURCES.md).
//!
//! Response-only replay: the recorded requests are opencode's serialization, not mare's. Every
//! interaction of a cassette is one model call, decoded on its own. The expected values were read
//! from the raw response bodies (not from mare's output), in mare's usage convention: `input` is
//! the prompt minus cached and cache-written tokens, `output` includes reasoning (Gemini: candidates
//! plus thoughts), cache reads and writes separate.

use super::*;
use crate::common::cassette::{Cassette, MatchMode};

fn opencode(rel: &str) -> Cassette {
    Cassette::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cassettes/opencode").join(rel),
    )
}

#[derive(Clone, Copy)]
enum Wire {
    Responses,
    Anthropic,
    Chat,
    Gemini,
}

impl Wire {
    /// The route `provider_host` gives the wire, and a model on it.
    fn route(self) -> (&'static str, &'static str) {
        match self {
            Self::Responses => ("/openai", "openai/gpt-6.1-terra"),
            Self::Anthropic => ("/anthropic", "anthropic/claude-sonnet-5-5"),
            Self::Chat => ("/compat", "deepseek/deepseek-chat"),
            Self::Gemini => ("/google", "google/gemini-2.5-pro"),
        }
    }
}

/// One interaction as it must decode.
struct Turn {
    /// the text, or with `text_len` its first characters
    text: &'static str,
    /// the whole text's length in chars, when `text` is only its start
    text_len: Option<usize>,
    /// the thinking text's length in chars, when the wire carries reasoning text
    thinking: Option<usize>,
    /// (name, arguments) in order
    calls: Vec<(&'static str, Value)>,
    stop: &'static str,
    /// input, output, cache_read, cache_write
    usage: [i64; 4],
}

fn turn(text: &'static str, stop: &'static str, usage: [i64; 4]) -> Turn {
    Turn { text, text_len: None, thinking: None, calls: vec![], stop, usage }
}

fn paris(name: &'static str) -> Vec<(&'static str, Value)> {
    vec![(name, json!({ "city": "Paris" }))]
}

/// Every interaction of `rel`, one call each, on a host with `llm.retries = retries`; the
/// cassette must be used up exactly (no retry asked for more, nothing left over).
async fn decode(rel: &str, wire: Wire, retries: u32) -> Vec<Value> {
    let mut env = TestEnv::new().await;
    let server = MockServer::start().await;
    let cassette = opencode(rel);
    let n = cassette.interactions.len();
    let (prefix, model) = wire.route();
    let replay = server.cassette(prefix, cassette, MatchMode::ResponseOnly);
    let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": retries } } })).await;
    let mut out = vec![];
    for _ in 0..n {
        out.push(ask(&h, model).await);
    }
    h.shutdown().await;
    replay.assert_done();
    out
}

fn check(rel: &str, i: usize, msg: &Value, want: &Turn) {
    let at = format!("{rel} #{i}");
    assert_eq!(msg["stop_reason"], want.stop, "{at}: {msg}");
    let text = text_of(msg);
    match want.text_len {
        None => assert_eq!(text, want.text, "{at}: {msg}"),
        Some(len) => {
            assert!(text.starts_with(want.text), "{at}: text starts {:?}: {msg}", want.text);
            assert_eq!(text.chars().count(), len, "{at}: text length: {msg}");
        }
    }
    let blocks = msg["content"].as_array().unwrap();
    let thinking: String =
        blocks.iter().filter(|b| b["type"] == "reasoning").filter_map(|b| b["text"].as_str()).collect();
    match want.thinking {
        Some(len) => assert_eq!(thinking.chars().count(), len, "{at}: thinking {thinking:?}"),
        None => assert!(thinking.is_empty(), "{at}: no reasoning text on the wire, got {thinking:?}"),
    }
    let calls: Vec<(String, Value)> = blocks
        .iter()
        .filter(|b| b["type"] == "tool_call" && b["hosted"].as_bool() != Some(true))
        .map(|b| (b["name"].as_str().unwrap_or_default().to_string(), b["arguments"].clone()))
        .collect();
    let expected: Vec<(String, Value)> = want.calls.iter().map(|(n, a)| (n.to_string(), a.clone())).collect();
    assert_eq!(calls, expected, "{at}: {msg}");
    let u = &msg["usage"];
    let got = [&u["input"], &u["output"], &u["cache_read"], &u["cache_write"]].map(|v| v.as_i64().unwrap_or(-1));
    assert_eq!(got, want.usage, "{at}: usage [input, output, cache_read, cache_write] {u}");
}

/// Text, reasoning, tool calls, stop reason and usage of each recorded interaction, wire by wire.
/// Cases (from the recordings): cache write then read (Anthropic, Responses), reasoning_content /
/// `reasoning` across tool loops (Z.ai, Groq gpt-oss, Cerebras), text and a tool call in one turn
/// (Z.ai), a whole tool call in one chunk (Mistral, Groq), content on the finish chunk (Mistral),
/// an indexed second argument chunk with no id and an empty name (Mistral-hosted GLM: "stop" with a
/// tool call is still a tool call — opencode mistral.recorded.test.ts:161), arguments on the finish
/// chunk and top-level `cached_tokens` (Together AI), usage in a trailing `choices: []` chunk with
/// arguments split mid-word (DigitalOcean), `\r\n\r\n` events, usage-only early chunks and a
/// thought signature on a trailing empty part (Vertex).
#[tokio::test]
async fn opencode_cassettes_decode_text_reasoning_tool_calls_and_usage() {
    local(async {
        let cases: Vec<(&str, Wire, Vec<Turn>)> = vec![
            (
                "anthropic-messages-cache/writes-then-reads-cache-control-on-identical-second-call.json",
                Wire::Anthropic,
                vec![turn("Hi.", "stop", [9, 5, 0, 5752]), turn("Hi.", "stop", [9, 5, 5752, 0])],
            ),
            (
                "openai-responses-cache/reports-cached-tokens-on-identical-second-call.json",
                Wire::Responses,
                vec![turn("Hi.", "stop", [4765, 3, 0, 0]), turn("Hi.", "stop", [157, 3, 4608, 0])],
            ),
            (
                "zai-chat/glm-5-3-preserves-reasoning-through-a-tool-loop-and-follow-up.json",
                Wire::Chat,
                vec![
                    Turn {
                        text_len: None,
                        text: "The total cost is 219 × $173 = $38,787... let me recalculate: 219 × 173 = $37,887, which is under the $38,000 budget. It's affordable, so I'll check the Paris weather now.",
                        thinking: Some(67),
                        calls: paris("get_weather"),
                        stop: "tool_use",
                        usage: [222, 96, 0, 0],
                    },
                    turn("It's currently sunny and 18°C in Paris.", "stop", [333, 13, 0, 0]),
                    turn("18°C", "stop", [361, 4, 0, 0]),
                ],
            ),
            (
                "groq-chat/replays-gpt-oss-reasoning-through-a-tool-loop.json",
                Wire::Chat,
                vec![
                    Turn { text: "", text_len: None, thinking: Some(63), calls: paris("lookup_weather"), stop: "tool_use", usage: [169, 37, 0, 0] },
                    turn("Paris is sunny with a temperature of 18°C.", "stop", [224, 15, 0, 0]),
                ],
            ),
            (
                "mistral-chat/drives-a-tool-loop.json",
                Wire::Chat,
                vec![
                    Turn { text: "", text_len: None, thinking: None, calls: paris("lookup_weather"), stop: "tool_use", usage: [120, 12, 0, 0] },
                    turn("The weather in Paris is currently sunny with a temperature of 18°C.", "stop", [67, 17, 0, 0]),
                ],
            ),
            (
                "mistral-chat-glm/streams-an-indexed-tool-call.json",
                Wire::Chat,
                vec![Turn { text: "", text_len: None, thinking: None, calls: paris("lookup_weather"), stop: "tool_use", usage: [171, 11, 0, 0] }],
            ),
            (
                "openai-compatible-chat/togetherai-streams-tool-call.json",
                Wire::Chat,
                vec![Turn { text: "", text_len: None, thinking: None, calls: paris("get_weather"), stop: "tool_use", usage: [194, 19, 0, 0] }],
            ),
            (
                "digitalocean-chat/haiku-continues-a-tool-call-with-cache-markers.json",
                Wire::Chat,
                vec![
                    Turn { text: "", text_len: None, thinking: None, calls: paris("get_weather"), stop: "tool_use", usage: [593, 54, 0, 0] },
                    turn("Paris is sunny.", "stop", [668, 7, 0, 0]),
                ],
            ),
            (
                "cerebras-chat/cerebras-gpt-oss-120b-tool-loop.json",
                Wire::Chat,
                vec![
                    Turn { text: "", text_len: None, thinking: Some(536), calls: paris("get_weather"), stop: "tool_use", usage: [150, 144, 0, 0] },
                    turn("Paris is sunny.", "stop", [64, 8, 128, 0]),
                ],
            ),
            (
                "google-vertex/continues-after-a-tool-result.json",
                Wire::Gemini,
                vec![turn("The weather in Paris is currently 18°C with light rain.", "stop", [59, 15, 0, 0])],
            ),
            ("google-vertex/streams-text.json", Wire::Gemini, vec![turn("Hello", "stop", [7, 278, 0, 0])]),
        ];
        run_cases(cases).await;
    })
    .await
}

async fn run_cases(cases: Vec<(&str, Wire, Vec<Turn>)>) {
    for (rel, wire, turns) in cases {
        let got = decode(rel, wire, 0).await;
        assert_eq!(got.len(), turns.len(), "{rel}");
        for (i, (msg, want)) in got.iter().zip(&turns).enumerate() {
            check(rel, i, msg, want);
        }
        let expected_sigs: &[(usize, Sig)] = match rel {
            "minimax-messages/m3-continues-a-tool-loop-with-adaptive-thinking.json" => &[
                (0, Sig::Thinking("61f13458806572210eb8e1809b12f191297eea1a7f0104aa12f86595ef5eb1dc")),
                (1, Sig::Thinking("fefe2c0e5f3f0a0edc24b21d37c8b7fb12ed0755bd1898f71acc2325f1b35958")),
            ],
            "venice-chat/claude-opus-4-8-preserves-reasoning-through-a-tool-loop-and-follow-up.json" => {
                &[(0, Sig::ThinkingLen(436)), (1, Sig::ThinkingLen(424))]
            }
            "openrouter-reasoning-tool-loop.json" => &[(0, Sig::ThinkingLen(424))],
            _ => &[],
        };
        for &(i, ref sig) in expected_sigs {
            let msg = &got[i];
            let blocks = msg["content"].as_array().unwrap();
            let thinking: String = blocks
                .iter()
                .filter(|b| b["type"] == "reasoning")
                .map(|b| {
                    if let Some(s) = b["native"]["anthropic-messages"]["signature"].as_str() {
                        s.to_string()
                    } else if let Some(details) = b["native"]["openai-chat"]["reasoning_details"].as_array() {
                        details.iter().filter_map(|d| d["signature"].as_str()).collect::<String>()
                    } else {
                        String::new()
                    }
                })
                .collect();
            let at = format!("{rel} #{i}");
            match sig {
                Sig::ThinkingLen(len) => assert_eq!(thinking.chars().count(), *len, "{at}: {msg}"),
                Sig::Thinking(exact) => assert_eq!(thinking, *exact, "{at}: {msg}"),
            }
        }
    }
}

/// Import batch 2 (SOURCES.md rows from moonshot-messages on), same properties. Cases (from the
/// recordings): thinking with an empty signature (Moonshot, DashScope Anthropic), signature only as
/// a later `signature_delta` (MiniMax), `redacted_thinking` / encrypted reasoning with no text
/// (Meta), reasoning as `response.reasoning_text.delta` with `msg_` ids and `:HTTP_STATUS` comment
/// lines (DashScope Responses), hosted web search / extraction items that produce no client calls,
/// `finish_reason: "stop"` with a tool call (DashScope Chat, DeepInfra), a tool-call id only on the
/// first fragment, usage on two finish chunks or a trailing `choices: []` chunk (Cloudflare,
/// DeepInfra, Venice, Moonshot), reasoning sent twice — `reasoning_content` / `reasoning` and
/// `reasoning_details[].text` — counted once (Venice Claude, OpenRouter), whole parallel calls with
/// a signature on the first only (Venice Gemini), duplicate finish chunks (OpenRouter, Cloudflare),
/// interim `message_start` usage replaced by `message_delta` (Azure, DashScope).
#[tokio::test]
async fn opencode_cassettes_batch_2_decode_text_reasoning_tool_calls_and_usage() {
    local(async {
        let cases: Vec<(&str, Wire, Vec<Turn>)> = vec![
        (
            "moonshot-messages/kimi-k3-preserves-reasoning-through-a-tool-loop-and-follow-up.json",
            Wire::Anthropic,
            vec![
                Turn { text: "", text_len: None, thinking: Some(83), calls: vec![("get_weather", json!({"city": "Paris"}))], stop: "tool_use", usage: [200, 69, 0, 0] },
                Turn { text: "It is sunny in Paris with a temperature of 18°C.", text_len: None, thinking: Some(31), calls: vec![], stop: "stop", usage: [55, 32, 0, 256] },
                Turn { text: "18C", text_len: None, thinking: Some(143), calls: vec![], stop: "stop", usage: [121, 47, 256, 0] },
            ],
        ),
        (
            "moonshot-responses/kimi-k3-preserves-reasoning-through-a-tool-loop-and-follow-up.json",
            Wire::Responses,
            vec![
                Turn { text: "", text_len: None, thinking: Some(103), calls: vec![("get_weather", json!({"city": "Paris"}))], stop: "tool_use", usage: [200, 74, 0, 0] },
                Turn { text: "It’s sunny and 18°C in Paris right now.", text_len: None, thinking: Some(26), calls: vec![], stop: "stop", usage: [316, 30, 0, 0] },
                Turn { text: "18C", text_len: None, thinking: Some(199), calls: vec![], stop: "stop", usage: [124, 65, 256, 0] },
            ],
        ),
        (
            "moonshot-chat/k3-returns-native-structured-output.json",
            Wire::Chat,
            vec![
                Turn { text: "{\"city\":\"Paris\"}", text_len: None, thinking: Some(173), calls: vec![], stop: "stop", usage: [184, 55, 0, 0] },
            ],
        ),
        (
            "alibaba-messages/qwen-3-8-max-replays-reasoning-through-a-tool-loop-and-follow-up.json",
            Wire::Anthropic,
            vec![
                Turn { text: "", text_len: None, thinking: Some(68), calls: vec![("get_weather", json!({"city": "Paris"}))], stop: "tool_use", usage: [351, 60, 0, 0] },
                Turn { text: "Yes, it's affordable: 219 trips × $173 = $37,887, which is within the $38,000 budget (leaving $113 to spare). Paris is currently sunny at 18°C.", text_len: None, thinking: Some(79), calls: vec![], stop: "stop", usage: [439, 98, 0, 0] },
                Turn { text: "18C", text_len: None, thinking: Some(43), calls: vec![], stop: "stop", usage: [562, 17, 0, 0] },
            ],
        ),
        (
            "alibaba-responses/qwen-3-8-max-replays-reasoning-through-a-tool-loop-and-follow-up.json",
            Wire::Responses,
            vec![
                Turn { text: "The total cost for 219 trips at $173 each is $37,887. Since $37,887 is less than your budget of $38,000, it is affordable. \n\nLet me look up the weather in Paris for you.\n\n", text_len: None, thinking: Some(1992), calls: vec![("get_weather", json!({"city": "Paris"}))], stop: "tool_use", usage: [368, 748, 0, 0] },
                Turn { text: "The weather in Paris is currently sunny and 18°C.", text_len: None, thinking: Some(769), calls: vec![], stop: "stop", usage: [1144, 227, 0, 0] },
                Turn { text: "18C", text_len: None, thinking: Some(498), calls: vec![], stop: "stop", usage: [372, 150, 1024, 0] },
            ],
        ),
        (
            "alibaba-chat/qwen-3-8-max-obeys-named-tool-choice.json",
            Wire::Chat,
            vec![
                Turn { text: "", text_len: None, thinking: None, calls: vec![("get_weather", json!({"city": "Paris"}))], stop: "tool_use", usage: [288, 19, 0, 0] },
            ],
        ),
        (
            "alibaba-responses/qwen-3-8-max-uses-hosted-web-search-and-extraction.json",
            Wire::Responses,
            vec![
                Turn { text: "## Summary: Alibaba Cloud Model Studio\n\n**Alibaba Cloud Model Studio** is a one-", text_len: Some(1587), thinking: Some(342), calls: vec![], stop: "stop", usage: [4869, 557, 0, 0] },
            ],
        ),
        (
            "meta-messages/replays-encrypted-thinking-through-a-tool-loop.json",
            Wire::Anthropic,
            vec![
                Turn { text: "I'll look up the current weather in Paris.", text_len: None, thinking: None, calls: vec![("lookup_weather", json!({"city": "Paris"}))], stop: "tool_use", usage: [564, 97, 0, 0] },
                Turn { text: "It's sunny in Paris right now.", text_len: None, thinking: None, calls: vec![], stop: "stop", usage: [152, 55, 0, 0] },
            ],
        ),
        (
            "meta-responses/replays-encrypted-reasoning-through-a-tool-loop.json",
            Wire::Responses,
            vec![
                Turn { text: "I'll look up Paris's weather now.", text_len: None, thinking: None, calls: vec![("lookup_weather", json!({"city": "Paris"}))], stop: "tool_use", usage: [457, 92, 113, 0] },
                Turn { text: "Paris is currently sunny with a temperature of 18°C.", text_len: None, thinking: None, calls: vec![], stop: "stop", usage: [198, 59, 497, 0] },
            ],
        ),
        (
            "venice-chat/gemini-preserves-per-call-signatures-through-parallel-tools.json",
            Wire::Chat,
            vec![
                Turn { text: "", text_len: None, thinking: None, calls: vec![("lookup_rate", json!({"code": "ZEBRA"})), ("lookup_rate", json!({"code": "MOOSE"}))], stop: "tool_use", usage: [74, 156, 0, 0] },
                Turn { text: "The retrieved rates are:\n- **ZEBRA:** 23\n- **MOOSE:** 31\n\n### Calculation:\n1. **Add the rates:**  \n   $$23 + 31 = 54$$\n\n2. **Multiply by 7:**  \n   $$54 \\times 7 = 378$$\n\nThe final result is **378**.", text_len: None, thinking: None, calls: vec![], stop: "stop", usage: [206, 208, 0, 0] },
                Turn { text: "385", text_len: None, thinking: None, calls: vec![], stop: "stop", usage: [429, 83, 0, 0] },
            ],
        ),
        (
            "venice-chat/claude-opus-4-8-preserves-reasoning-through-a-tool-loop-and-follow-up.json",
            Wire::Chat,
            vec![
                Turn { text: "", text_len: None, thinking: Some(36), calls: vec![("lookup_rate", json!({"code": "ZEBRA"}))], stop: "tool_use", usage: [499, 74, 0, 0] },
                Turn { text: "400", text_len: None, thinking: Some(39), calls: vec![], stop: "stop", usage: [183, 23, 0, 0] },
                Turn { text: "407", text_len: None, thinking: None, calls: vec![], stop: "stop", usage: [229, 3, 0, 0] },
            ],
        ),
        (
            "openrouter-reasoning-tool-loop.json",
            Wire::Chat,
            vec![
                Turn { text: "Sure! Let me check the weather in Paris for you right now!", text_len: None, thinking: Some(75), calls: vec![("get_weather", json!({"city": "Paris"}))], stop: "tool_use", usage: [623, 95, 0, 0] },
                Turn { text: "Paris is sunny.", text_len: None, thinking: None, calls: vec![], stop: "stop", usage: [739, 7, 0, 0] },
            ],
        ),
        (
            "cloudflare-workers-ai/cloudflare-workers-ai-gpt-oss-20b-tools-tool-call.json",
            Wire::Chat,
            vec![
                Turn { text: "", text_len: None, thinking: Some(59), calls: vec![("get_weather", json!({"city": "Paris"}))], stop: "tool_use", usage: [136, 37, 0, 0] },
            ],
        ),
        (
            "deepinfra-chat/deepinfra-llama-3-3-70b-tool-loop.json",
            Wire::Chat,
            vec![
                Turn { text: "", text_len: None, thinking: None, calls: vec![("get_weather", json!({"city": "Paris"}))], stop: "tool_use", usage: [243, 14, 0, 0] },
                Turn { text: "Paris is sunny.", text_len: None, thinking: None, calls: vec![], stop: "stop", usage: [276, 5, 0, 0] },
            ],
        ),
        (
            "minimax-messages/m3-continues-a-tool-loop-with-adaptive-thinking.json",
            Wire::Anthropic,
            vec![
                Turn { text: "", text_len: None, thinking: Some(181), calls: vec![("get_weather", json!({"city": "Paris"}))], stop: "tool_use", usage: [239, 68, 203, 0] },
                Turn { text: "It's currently sunny in Paris with a temperature of 18°C.", text_len: None, thinking: Some(96), calls: vec![], stop: "stop", usage: [169, 38, 128, 0] },
            ],
        ),
        (
            "azure/responses-continues-after-a-tool-result.json",
            Wire::Responses,
            vec![
                Turn { text: "Paris is currently **18°C with light rain**.", text_len: None, thinking: None, calls: vec![], stop: "stop", usage: [86, 15, 0, 0] },
            ],
        ),
        (
            "azure/messages-calls-a-tool.json",
            Wire::Anthropic,
            vec![
                Turn { text: "", text_len: None, thinking: None, calls: vec![("lookup_weather", json!({"city": "Paris"}))], stop: "tool_use", usage: [573, 54, 0, 0] },
            ],
        ),

        ];
        run_cases(cases).await;
    })
    .await
}

/// What a signature must look like after decoding, for replay on the next turn.
enum Sig {
    /// the thinking blocks' signatures joined: exactly this many chars
    ThinkingLen(usize),
    /// the thinking blocks' signatures joined: exactly this
    Thinking(&'static str),
}

/// A gateway that reports Anthropic-style cache counts on Chat
/// Completions (DigitalOcean: top-level `cache_created_input_tokens`, `cache_read_input_tokens`)
/// — the written tokens are cache writes, not plain input.
#[tokio::test]
async fn a_gateways_anthropic_style_cache_writes_are_cache_writes() {
    local(run_cases(vec![(
        "digitalocean-chat/haiku-reuses-a-cached-prompt.json",
        Wire::Chat,
        vec![turn("OK", "stop", [3, 4, 0, 5759]), turn("OK", "stop", [3, 4, 5759, 0])],
    )]))
    .await
}

/// A provider's error body reaches the caller as written, and a 4xx that is not a rate limit is not
/// retried even with retries left (the cassette has one interaction: a retry would find none).
/// Venice 404 `{"error": "<string>"}` (no type, no code), Vercel gateway 401 on `/v1/messages`
/// without Anthropic's error envelope (opencode vercel-native: not retryable), Moonshot 400
/// `invalid_request_error` for `tool_choice: "required"`.
#[tokio::test]
async fn opencode_cassettes_error_bodies_reach_the_caller_without_retry() {
    local(async {
        let cases = [
            (
                "venice-chat/surfaces-venice-model-errors.json",
                Wire::Chat,
                "Specified model not found: no-such-model-xyz. Did you mean: z-ai-glm-5-3, z-ai-glm-5-3-flash, z-ai-glm-5-turbo?",
            ),
            (
                "vercel-native/rejects-invalid-credentials-without-retryable-classification.json",
                Wire::Anthropic,
                "Authentication failed. Create an API key and set in AI_GATEWAY_API_KEY environment variable",
            ),
            (
                "moonshot-responses/k3-rejects-unsupported-tool-choice-required.json",
                Wire::Responses,
                r#"unsupported tool_choice value: "required""#,
            ),
        ];
        for (rel, wire, message) in cases {
            let got = decode(rel, wire, 2).await;
            let msg = &got[0];
            assert_eq!(msg["stop_reason"], "error", "{rel}: {msg}");
            assert!(msg["error"].as_str().is_some_and(|e| e.contains(message)), "{rel}: the provider's message is lost: {msg}");
        }
    })
    .await
}

/// (opencode http-recorder test/http.test.ts:50: a wrong request does not consume
/// an interaction): with request matching, a recorded request serves only the request it recorded.
/// mare's real request is captured first; replayed against itself it is served, and the same
/// request with only `model` changed is refused with the JSON path that differs, and nothing is
/// consumed.
#[tokio::test]
async fn request_matching_serves_only_the_recorded_request() {
    local(async {
        let rel = "openai-compatible-chat/togetherai-streams-tool-call.json";
        let (prefix, model) = Wire::Chat.route();
        // mare's own request for this call
        let sent = {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let replay = server.cassette(prefix, opencode(rel), MatchMode::ResponseOnly);
            let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;
            ask(&h, model).await;
            h.shutdown().await;
            replay.assert_done();
            server.requests().into_iter().find(|r| r.path.starts_with(prefix)).expect("a request")
        };
        let with_request = |body: &str| -> Cassette {
            let mut c = serde_json::to_value(opencode(rel)).unwrap();
            c["interactions"][0]["request"] =
                json!({ "method": "POST", "url": format!("https://example.test{}", sent.path), "body": body });
            serde_json::from_value(c).unwrap()
        };
        let mut wrong: Value = serde_json::from_str(&sent.body).unwrap();
        wrong["model"] = json!("some-other-model");
        for (cassette, served) in [(with_request(&sent.body), true), (with_request(&wrong.to_string()), false)] {
            assert_eq!(cassette.mode(), MatchMode::RequestAndResponse);
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let replay = server.cassette(prefix, cassette, MatchMode::RequestAndResponse);
            let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;
            let msg = ask(&h, model).await;
            h.shutdown().await;
            if served {
                assert_eq!(msg["stop_reason"], "tool_use", "{msg}");
                replay.assert_done();
            } else {
                assert_eq!(msg["stop_reason"], "error", "{msg}");
                let unmatched = replay.unmatched().join("\n");
                assert!(unmatched.contains("$.model"), "the miss names the differing field: {unmatched}");
            }
        }
    })
    .await
}

/// The assistant message of mare's second request (turn 1 decoded from the cassette's first
/// interaction, its tool calls answered `{}`, turn 2 served the second interaction), and the
/// assistant message of the request opencode recorded for that second interaction.
async fn second_turn(rel: &str) -> (Value, Value) {
    let mut env = TestEnv::new().await;
    let server = MockServer::start().await;
    let cassette = opencode(rel);
    let recorded: Value =
        serde_json::from_str(&cassette.interactions[1].request.as_ref().expect("a recorded request").body).unwrap();
    // a gateway endpoint (OpenRouter's, pointed at the mock), not DeepSeek, whose own rule adds a
    // placeholder reasoning_content to every tool turn
    let (prefix, model) = ("/gateway", "openrouter/anthropic/claude-sonnet-5-5");
    env.set_var("OPENROUTER_API_KEY", "test-openrouter");
    let replay = server.cassette(prefix, cassette, MatchMode::ResponseOnly);
    let gateway = json!({ "base_url": format!("{}{prefix}", server.base) });
    let h = provider_host(
        &mut env,
        &server,
        json!({ "providers": { "openrouter": gateway }, "plugins": { "llm": { "retries": 0 } } }),
    )
    .await;
    global(&h, "MODEL", model);
    let _: mlua::Value = eval(
        &h,
        r#"
local m = assert(LLM:resolve(MODEL))
local tool = function(name) return { name = name, description = name, parameters = { type = "object", properties = {} } } end
local tools = { tool("lookup_rate"), tool("lookup_weather"), tool("get_weather") }
local user = { role = "user", content = { { type = "text", text = "go" } } }
local m1 = LLM:stream({ model = m, tools = tools, messages = { user } }, function() end)
local history = { user, m1 }
for _, b in ipairs(m1.content) do
  if b.type == "tool_call" then
    history[#history + 1] = { role = "tool_result", tool_call_id = b.id, tool_name = b.name, content = { { type = "text", text = "{}" } } }
  end
end
return LLM:stream({ model = m, tools = tools, messages = history }, function() end)
"#,
    )
    .await;
    h.shutdown().await;
    let assistant = |body: &Value| -> Value {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .cloned()
            .expect("an assistant message")
    };
    let sent: Value = serde_json::from_str(
        &server.requests().iter().filter(|r| r.path.starts_with(prefix)).nth(1).expect("a second request").body,
    )
    .unwrap();
    let _ = replay;
    (assistant(&sent), assistant(&recorded))
}

/// What a gateway needs back on the next tool turn goes back as it came, compared with the request
/// opencode recorded for the same turn: a Gemini thought signature directly on the call it belongs
/// to and on no other (Venice); the Claude thinking signature, format and id in
/// `reasoning_details` beside the reasoning text (Venice, OpenRouter); and the reasoning text in the
/// field it arrived in — `reasoning` for OpenRouter, Groq and Cerebras (terra backlog ②),
/// `reasoning_content` for Venice.
#[tokio::test]
async fn signatures_and_reasoning_go_back_as_the_gateway_sent_them() {
    local(async {
        let pick = |m: &Value, path: &[&str]| -> Value {
            path.iter().fold(m.clone(), |v, k| match k.parse::<usize>() {
                Ok(i) => v[i].clone(),
                Err(_) => v[*k].clone(),
            })
        };
        let cases: [(&str, &[&[&str]]); 7] = [
            (
                "venice-chat/gemini-preserves-per-call-signatures-through-parallel-tools.json",
                &[
                    &["tool_calls", "0", "thought_signature"],
                    &["tool_calls", "1", "thought_signature"],
                    &["tool_calls", "0", "extra_content"],
                ],
            ),
            (
                "venice-chat/claude-opus-4-8-preserves-reasoning-through-a-tool-loop-and-follow-up.json",
                &[&["reasoning_content"], &["reasoning"], &["reasoning_details"]],
            ),
            ("openrouter-reasoning-tool-loop.json", &[&["reasoning"], &["reasoning_content"], &["reasoning_details"]]),
            (
                "venice-chat/gemini-3-1-pro-preview-preserves-reasoning-through-a-tool-loop-and-follow-up.json",
                &[&["reasoning_details"], &["reasoning_content"], &["tool_calls"]],
            ),
            (
                "venice-chat/openai-gpt-54-mini-preserves-reasoning-through-a-tool-loop-and-follow-up.json",
                &[&["reasoning_details"], &["reasoning_content"]],
            ),
            ("groq-chat/replays-gpt-oss-reasoning-through-a-tool-loop.json", &[&["reasoning"], &["reasoning_content"]]),
            ("cerebras-chat/cerebras-gpt-oss-120b-tool-loop.json", &[&["reasoning"], &["reasoning_content"]]),
        ];
        for (rel, paths) in cases {
            let (sent, recorded) = second_turn(rel).await;
            for path in paths {
                let want = pick(&recorded, path);
                assert_eq!(pick(&sent, path), want, "{rel}: {} in {sent}", path.join("."));
            }
        }
    })
    .await
}

/// reference implementation checks signatures on the original blocks after replay;
/// this three-turn Venice recording additionally places a text signature on a contentless tail.
#[tokio::test]
async fn venice_text_signature_survives_the_recorded_three_turns() {
    local(async {
        let rel = "venice-chat/gemini-preserves-per-call-signatures-through-parallel-tools.json";
        let cassette = opencode(rel);
        let response_signature = cassette.interactions[1]
            .response
            .body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .find_map(|event| event["choices"][0]["delta"]["thought_signature"].as_str().map(str::to_owned))
            .expect("the second response has a text signature");
        assert_eq!(response_signature.len(), 628);
        let recorded: Value = serde_json::from_str(&cassette.interactions[2].request.as_ref().unwrap().body).unwrap();
        let recorded_assistants: Vec<_> = recorded["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "assistant").collect();
        assert_eq!(recorded_assistants.len(), 2);
        assert_eq!(recorded_assistants[1]["thought_signature"].as_str(), Some(response_signature.as_str()));

        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        env.set_var("OPENROUTER_API_KEY", "test-openrouter");
        let replay = server.cassette("/gateway", cassette, MatchMode::ResponseOnly);
        let gateway = json!({ "base_url": format!("{}/gateway", server.base) });
        let h = provider_host(
            &mut env,
            &server,
            json!({ "providers": { "openrouter": gateway }, "plugins": { "llm": { "retries": 0 } } }),
        )
        .await;
        let _: mlua::Value = eval(
            &h,
            r#"
local model = assert(LLM:resolve("openrouter/anthropic/claude-sonnet-5-5"))
local function tool(name) return { name = name, description = name, parameters = { type = "object", properties = {} } } end
local tools = { tool("lookup_rate"), tool("lookup_weather"), tool("get_weather") }
local user = { role = "user", content = { { type = "text", text = "go" } } }
local first = LLM:stream({ model = model, tools = tools, messages = { user } }, function() end)
local history = { user, first }
for _, block in ipairs(first.content) do
  if block.type == "tool_call" then
    history[#history + 1] = { role = "tool_result", tool_call_id = block.id, tool_name = block.name, content = { { type = "text", text = "{}" } } }
  end
end
local second = LLM:stream({ model = model, tools = tools, messages = history }, function() end)
history[#history + 1] = second
history[#history + 1] = { role = "user", content = { { type = "text", text = "again" } } }
return LLM:stream({ model = model, tools = tools, messages = history }, function() end)
"#,
        )
        .await;
        h.shutdown().await;
        replay.assert_done();
        let sent: Value = serde_json::from_str(&server.requests()[2].body).unwrap();
        let assistants: Vec<_> = sent["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "assistant").collect();
        assert_eq!(assistants.len(), 2);
        assert_eq!(assistants[0]["tool_calls"], recorded_assistants[0]["tool_calls"], "parallel tool signatures stay on their own calls");
        assert_eq!(assistants[1]["thought_signature"].as_str(), Some(response_signature.as_str()), "text signature on the third request");
    })
    .await
}

#[tokio::test]
async fn venice_text_signature_only_returns_to_its_source() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let signed = compat_events(&[
            compat_text("first "),
            compat_text("answer"),
            json!({ "choices": [{ "delta": { "thought_signature": "text-signature" } }] }),
            compat_stop(),
        ]);
        let unsigned = compat_events(&[compat_text("unsigned"), compat_stop()]);
        server.route(
            "/gateway",
            vec![
                Resp::sse(signed),
                Resp::sse(&unsigned),
                Resp::sse(&unsigned),
                Resp::sse(&unsigned),
                Resp::sse(&unsigned),
            ],
        );
        server.route("/compat", vec![Resp::sse(unsigned)]);
        env.set_var("OPENROUTER_API_KEY", "test-openrouter");
        let gateway = json!({ "base_url": format!("{}/gateway", server.base) });
        let h = provider_host(
            &mut env,
            &server,
            json!({ "providers": { "openrouter": gateway }, "plugins": { "llm": { "retries": 0 } } }),
        )
        .await;
        let _: mlua::Value = eval(
            &h,
            r#"
local original = assert(LLM:resolve("openrouter/anthropic/claude-sonnet-5-5"))
local other_model = assert(LLM:resolve("openrouter/deepseek/deepseek-chat"))
local foreign = assert(LLM:resolve("deepseek/deepseek-chat"))
local user = { role = "user", content = { { type = "text", text = "go" } } }
local function send(model, history)
  return LLM:stream({ model = model, messages = history }, function() end)
end
local signed = send(original, { user })
local unsigned = send(original, { user, signed, user })
send(foreign, { user, signed, user })
send(other_model, { user, signed, user })
send(original, { user, unsigned, user })
local wrong_source = { role = "assistant", provider_id = original.provider, model_id = original.id,
  native = { ["other-wire"] = { thought_signature = "wrong-source" } },
  content = { { type = "text", text = "reply" } } }
return send(original, { user, wrong_source, user })
"#,
        )
        .await;
        h.shutdown().await;
        let requests = server.requests();
        let gateway: Vec<_> = requests.iter().filter(|r| r.path.starts_with("/gateway")).map(|r| r.json()).collect();
        let foreign = requests.iter().find(|r| r.path.starts_with("/compat")).unwrap().json();
        assert_eq!(gateway.len(), 5);
        let assistant = |body: &Value| {
            body["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap().clone()
        };
        assert_eq!(assistant(&gateway[1])["thought_signature"], "text-signature");
        for body in [&foreign, &gateway[2], &gateway[3], &gateway[4]] {
            assert!(
                assistant(body).get("thought_signature").is_none(),
                "foreign, mismatched or unsigned history: {body}"
            );
        }
    })
    .await
}

/// Persist the recorded text response through the real session file and resume it for the next request.
#[tokio::test]
async fn venice_text_signature_survives_session_resume() {
    local(async {
        let cassette = opencode("venice-chat/gemini-preserves-per-call-signatures-through-parallel-tools.json");
        let recorded: Value = serde_json::from_str(&cassette.interactions[2].request.as_ref().unwrap().body).unwrap();
        let signature = recorded["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|m| m["thought_signature"].as_str())
            .unwrap()
            .to_owned();
        assert_eq!(signature.len(), 628);
        let mut env = TestEnv::new().await;
        env.set_var("OPENROUTER_API_KEY", "test-openrouter");
        let server = MockServer::start().await;
        server.route(
            "/gateway",
            vec![
                Resp::sse(cassette.interactions[1].response.body.clone()),
                Resp::sse(cassette.interactions[2].response.body.clone()),
            ],
        );
        let config = json!({
            "model": "openrouter/anthropic/claude-sonnet-5-5",
            "providers": { "openrouter": { "base_url": format!("{}/gateway", server.base) } },
            "plugins": { "llm": { "retries": 0 } }
        });
        let h = env.host().config(config.clone()).build_exposed().await;
        let first = h.prompt("start", json!({})).await.unwrap();
        assert!(first["error"].is_null(), "{first}");
        let path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;
        let records = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
        assert!(records.iter().any(|r| r.to_string().contains(&signature)), "signature must be written to the session");

        let resumed = env.host().config(config).resume().build_exposed().await;
        let reply = resumed.prompt("again", json!({})).await.unwrap();
        assert!(reply["error"].is_null(), "{reply}");
        resumed.shutdown().await;
        let sent = server.requests()[1].json();
        let assistant = sent["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(assistant["thought_signature"].as_str(), Some(signature.as_str()));
    })
    .await
}

/// The assistant message mare sends back on the turn after `first` (a compat stream), and the
/// decoded first message — on the gateway endpoint (OpenRouter's, at the mock).
async fn replay_after(first: Vec<Value>) -> (Value, Value) {
    let (sent, message, _) = replay_after_with_thinking_deltas(first).await;
    (sent, message)
}

async fn replay_after_with_thinking_deltas(first: Vec<Value>) -> (Value, Value, String) {
    let mut env = TestEnv::new().await;
    let server = MockServer::start().await;
    server.route(
        "/gateway",
        vec![Resp::sse(compat_events(&first)), Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]))],
    );
    env.set_var("OPENROUTER_API_KEY", "test-openrouter");
    let gateway = json!({ "base_url": format!("{}/gateway", server.base) });
    let h = provider_host(
        &mut env,
        &server,
        json!({ "providers": { "openrouter": gateway }, "plugins": { "llm": { "retries": 0 } } }),
    )
    .await;
    let m1: mlua::Value = eval(
        &h,
        r#"
local m = assert(LLM:resolve("openrouter/anthropic/claude-sonnet-5-5"))
local user = { role = "user", content = { { type = "text", text = "go" } } }
local thinking = {}
local M1 = LLM:stream({ model = m, messages = { user } }, function(ev)
    if ev.type == "reasoning_delta" then thinking[#thinking + 1] = ev.delta end
end)
LLM:stream({ model = m, messages = { user, M1, { role = "user", content = { { type = "text", text = "again" } } } } }, function() end)
return { message = M1, thinking_deltas = table.concat(thinking) }"#,
    )
    .await;
    let m1 = mare_shared::lua::lua_to_json(h.lua(), m1).unwrap();
    h.shutdown().await;
    let sent: Value = serde_json::from_str(&server.requests()[1].body).unwrap();
    let assistant = sent["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").cloned().unwrap();
    (assistant, m1["message"].clone(), m1["thinking_deltas"].as_str().unwrap().to_string())
}

/// opencode openai-chat.test.ts:1547 ("uses reasoning details as display fallback without inventing
/// a scalar replay field", review): a `reasoning.summary` entry and a
/// `reasoning.encrypted` one, with no scalar reasoning field — the summary is the shown thinking, and
/// the next request carries exactly these entries (summary text included) and no `reasoning` /
/// `reasoning_content`. And :1569: an entry of an unknown type is not echoed back.
#[tokio::test]
async fn reasoning_details_of_every_kind_go_back_whole() {
    local(async {
        let details = json!([
            { "type": "reasoning.summary", "summary": "thinking", "format": "openai-responses-v1", "index": 0 },
            { "type": "reasoning.encrypted", "data": "opaque", "format": "openai-responses-v1", "index": 1 },
        ]);
        let (sent, m1) = replay_after(vec![
            json!({ "choices": [{ "delta": { "reasoning_details": [details[0]] } }] }),
            json!({ "choices": [{ "delta": { "reasoning_details": [details[1]] } }] }),
            compat_text("Hello"),
            compat_stop(),
        ])
        .await;
        let shown: String = m1["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "reasoning")
            .filter_map(|b| b["text"].as_str())
            .collect();
        assert_eq!(shown, "thinking", "the summary is the shown reasoning: {m1}");
        assert_eq!(sent["reasoning_details"], details, "{sent}");
        assert!(
            sent.get("reasoning").is_none() && sent.get("reasoning_content").is_none(),
            "no invented scalar field: {sent}"
        );

        let (sent, _) = replay_after(vec![
            json!({ "choices": [{ "delta": { "reasoning": "thinking", "reasoning_details": [
                { "type": "reasoning.future", "format": "provider-v2", "state": { "opaque": true } }
            ] } }] }),
            compat_text("Hello"),
            compat_stop(),
        ])
        .await;
        assert_eq!(sent["reasoning"], "thinking", "{sent}");
        assert!(
            sent["reasoning_details"].as_array().is_none_or(|a| a.is_empty()),
            "an unknown detail is not echoed: {sent}"
        );
    })
    .await
}

/// opencode openai-chat.ts:1042-1074: encrypted details are opaque, even if identities repeat.
#[tokio::test]
async fn independent_encrypted_reasoning_details_are_replayed_in_order() {
    local(async {
        let details = json!([
            { "type": "reasoning.encrypted", "id": "a", "data": "X" },
            { "type": "reasoning.encrypted", "id": "b", "data": "Y" },
        ]);
        let (sent, _) = replay_after(vec![
            json!({ "choices": [{ "delta": { "reasoning_details": [details[0]] } }] }),
            json!({ "choices": [{ "delta": { "reasoning_details": [details[1]] } }] }),
            compat_text("Hello"),
            compat_stop(),
        ])
        .await;
        assert_eq!(sent["reasoning_details"], details, "{sent}");

        let repeated_identity = json!([
            { "type": "reasoning.encrypted", "id": "a", "index": 0, "data": "X" },
            { "type": "reasoning.encrypted", "id": "a", "index": 0, "data": "Y" },
        ]);
        let (sent, _) = replay_after(vec![
            json!({ "choices": [{ "delta": { "reasoning_details": [repeated_identity[0]] } }] }),
            json!({ "choices": [{ "delta": { "reasoning_details": [repeated_identity[1]] } }] }),
            compat_text("Hello"),
            compat_stop(),
        ])
        .await;
        assert_eq!(sent["reasoning_details"], repeated_identity, "{sent}");
    })
    .await
}

/// opencode openai-chat.ts:1042-1104, openai-chat.test.ts:1729-1775 and 1877-1891:
/// consecutive text or summary pieces merge unless their identities conflict.
#[tokio::test]
async fn reasoning_detail_text_merges_only_compatible_consecutive_entries() {
    local(async {
        let deltas = [
            json!({ "type": "reasoning.text", "text": "a", "id": "a", "index": 0, "format": "fmt" }),
            json!({ "type": "reasoning.text", "text": "b", "id": "a", "index": 0, "format": "fmt", "signature": "sig" }),
            json!({ "type": "reasoning.text", "text": "c", "id": "a", "index": 0, "format": "fmt", "signature": "sig" }),
            json!({ "type": "reasoning.text", "text": "d", "id": "b", "index": 0, "format": "fmt", "signature": "sig" }),
            json!({ "type": "reasoning.text", "text": "e", "id": "b", "index": 1, "format": "fmt", "signature": "sig" }),
            json!({ "type": "reasoning.text", "text": "f", "id": "b", "index": 1, "format": "other", "signature": "sig" }),
            json!({ "type": "reasoning.text", "text": "g", "id": "b", "index": 1, "format": "other", "signature": "sig2" }),
            json!({ "type": "reasoning.summary", "summary": "plan", "index": 1 }),
            json!({ "type": "reasoning.summary", "summary": " now", "index": 1 }),
            json!({ "type": "reasoning.text", "text": "h", "id": "a", "index": 0, "format": "fmt" }),
        ];
        let mut frames: Vec<Value> = deltas
            .iter()
            .map(|d| json!({ "choices": [{ "delta": { "reasoning_details": [d] } }] }))
            .collect();
        frames.push(compat_text("Hello"));
        frames.push(compat_stop());
        let (sent, _) = replay_after(frames).await;
        assert_eq!(sent["reasoning_details"], json!([
            { "type": "reasoning.text", "text": "abc", "id": "a", "index": 0, "format": "fmt", "signature": "sig" },
            { "type": "reasoning.text", "text": "d", "id": "b", "index": 0, "format": "fmt", "signature": "sig" },
            { "type": "reasoning.text", "text": "e", "id": "b", "index": 1, "format": "fmt", "signature": "sig" },
            { "type": "reasoning.text", "text": "f", "id": "b", "index": 1, "format": "other", "signature": "sig" },
            { "type": "reasoning.text", "text": "g", "id": "b", "index": 1, "format": "other", "signature": "sig2" },
            { "type": "reasoning.summary", "summary": "plan now", "index": 1 },
            { "type": "reasoning.text", "text": "h", "id": "a", "index": 0, "format": "fmt" },
        ]), "{sent}");
    })
    .await
}

/// A later piece of the same text entry whose `text` is not a string is skipped: the response
/// completes and the text received so far is kept (opencode decodes before merging,
/// openai-chat.ts:152).
#[tokio::test]
async fn a_detail_piece_of_the_wrong_shape_is_skipped_and_the_text_kept() {
    local(async {
        let frames = vec![
            json!({ "choices": [{ "delta": { "reasoning_details": [
                { "type": "reasoning.text", "text": "plan", "index": 0 }
            ] } }] }),
            json!({ "choices": [{ "delta": { "reasoning_details": [
                { "type": "reasoning.text", "text": { "not": "a string" }, "index": 0 }
            ] } }] }),
            compat_text("answer"),
            compat_stop(),
        ];
        let (sent, _) = replay_after(frames).await;
        assert_eq!(sent["content"], "answer", "{sent}");
        assert_eq!(
            sent["reasoning_details"],
            json!([{ "type": "reasoning.text", "text": "plan", "index": 0 }]),
            "{sent}"
        );
    })
    .await
}

/// Scalar reasoning is replayed once across frames; if it arrives first, it is shown once too.
#[tokio::test]
async fn scalar_reasoning_is_replayed_once_and_shown_once_when_first() {
    local(async {
        let scalar = json!({ "choices": [{ "delta": { "reasoning": "plan" } }] });
        let detail = json!({ "choices": [{ "delta": { "reasoning_details": [
            { "type": "reasoning.text", "text": "plan", "index": 0 }
        ] } }] });
        for (mut frames, has_scalar, check_display) in [
            (vec![scalar.clone(), detail.clone()], true, true),
            (vec![detail.clone(), scalar], true, false),
            (vec![detail], false, true),
        ] {
            frames.push(compat_text("Hello"));
            frames.push(compat_stop());
            let (sent, m1, shown_deltas) = replay_after_with_thinking_deltas(frames).await;
            let shown: String = m1["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["type"] == "reasoning")
                .filter_map(|b| b["text"].as_str())
                .collect();
            if has_scalar {
                assert_eq!(sent["reasoning"], "plan", "{sent}");
            } else {
                assert!(sent.get("reasoning").is_none() && sent.get("reasoning_content").is_none(), "{sent}");
            }
            assert_eq!(shown, "plan", "{m1}");
            if check_display {
                assert_eq!(shown_deltas, "plan", "streamed thinking for {m1}");
            }
            assert_eq!(
                sent["reasoning_details"],
                json!([{ "type": "reasoning.text", "text": "plan", "index": 0 }]),
                "{sent}"
            );
        }
    })
    .await
}

/// Details-only thinking reaches the sink as each frame arrives, even if the stream is interrupted.
#[tokio::test]
async fn reasoning_details_stream_immediately_without_a_scalar() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let streamed: bool = eval(
            &h,
            r#"
local COMPAT = require("provider-openai-compat.wire")
local events = {}
local p = COMPAT.new_parser(function(ev) events[#events + 1] = ev end)
local function feed(text)
    local detail = {type = "reasoning.text", text = text, index = 0}
    local delta = {reasoning_details = {detail}}
    p.feed({data = mare.json.encode({choices = {{delta = delta}}})})
end
feed("pl")
assert(#events == 1 and events[1].type == "reasoning_delta" and events[1].delta == "pl")
feed("an")
assert(#events == 2 and events[2].type == "reasoning_delta" and events[2].delta == "an")
local ok = pcall(p.finish_message) -- incomplete stream
assert(not ok and #events == 2)
return true
"#,
        )
        .await;
        assert!(streamed);
        h.shutdown().await;
        drop(env);

        let (sent, m1, shown_deltas) = replay_after_with_thinking_deltas(vec![
            json!({ "choices": [{ "delta": { "reasoning_details": [
                { "type": "reasoning.text", "text": "pl", "index": 0 }
            ] } }] }),
            json!({ "choices": [{ "delta": { "reasoning_details": [
                { "type": "reasoning.text", "text": "an", "index": 0 }
            ] } }] }),
            compat_text("Hello"),
            compat_stop(),
        ])
        .await;
        assert_eq!(shown_deltas, "plan", "{m1}");
        assert_eq!(sent["reasoning_details"], json!([{ "type": "reasoning.text", "text": "plan", "index": 0 }]));
        assert!(sent.get("reasoning").is_none() && sent.get("reasoning_content").is_none(), "{sent}");
    })
    .await
}
