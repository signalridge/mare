# opencode recordings (cassette v1, unchanged)

Upstream: `anomalyco/opencode` @ `0e447fc`, `packages/ai/test/fixtures/recordings/<same relative path>`.
License: MIT, Copyright (c) 2025 opencode (`LICENSE`; `packages/http-recorder/LICENSE` is the same MIT text).
Type: Real recording (opencode's HTTP recorder; its redactor and secret scan ran before upload).

Files are copied byte for byte; the relative path here is the upstream one. They are replayed
response-only (`MatchMode::ResponseOnly`): the request bodies are opencode's own serialization,
not mare's, so they are kept for provenance only.

Selection (one cassette per decoder path across the four supported wires: OpenAI Responses, Anthropic Messages, OpenAI-compatible Chat, Gemini; excluding Bedrock, WebSocket, and video):

| File | Wire | What it exercises |
|---|---|---|
| anthropic-messages-cache/writes-then-reads-cache-control-on-identical-second-call.json | Anthropic | cache write then cache read usage over two calls |
| openai-responses-cache/reports-cached-tokens-on-identical-second-call.json | Responses | cached input tokens on the second call |
| venice-chat/surfaces-venice-model-errors.json | Chat | HTTP 404 model error body |
| vercel-native/rejects-invalid-credentials-without-retryable-classification.json | Anthropic | HTTP 401, not retried |
| moonshot-responses/k3-rejects-unsupported-tool-choice-required.json | Responses | HTTP 400 tool_choice=required rejection |
| zai-chat/glm-5-3-preserves-reasoning-through-a-tool-loop-and-follow-up.json | Chat | reasoning_content across a tool loop |
| groq-chat/replays-gpt-oss-reasoning-through-a-tool-loop.json | Chat | gpt-oss reasoning field over a tool loop |
| mistral-chat/drives-a-tool-loop.json | Chat | Mistral tool loop |
| mistral-chat-glm/streams-an-indexed-tool-call.json | Chat | indexed tool-call deltas |
| openai-compatible-chat/togetherai-streams-tool-call.json | Chat | Together AI tool-call stream |
| google-vertex/continues-after-a-tool-result.json | Gemini | continuation after a function response |
| google-vertex/streams-text.json | Gemini | plain text stream |
| digitalocean-chat/haiku-continues-a-tool-call-with-cache-markers.json | Chat | cache usage fields on an OpenAI-compatible gateway |
| cerebras-chat/cerebras-gpt-oss-120b-tool-loop.json | Chat | Cerebras gpt-oss tool loop |
| moonshot-messages/kimi-k3-preserves-reasoning-through-a-tool-loop-and-follow-up.json | Anthropic | Kimi K3 thinking across a tool loop (Moonshot's Anthropic endpoint) |
| moonshot-responses/kimi-k3-preserves-reasoning-through-a-tool-loop-and-follow-up.json | Responses | Kimi K3 reasoning items across a tool loop |
| moonshot-chat/k3-returns-native-structured-output.json | Chat | native structured output as text |
| alibaba-messages/qwen-3-8-max-replays-reasoning-through-a-tool-loop-and-follow-up.json | Anthropic | Qwen thinking across a tool loop (DashScope Anthropic endpoint) |
| alibaba-responses/qwen-3-8-max-replays-reasoning-through-a-tool-loop-and-follow-up.json | Responses | Qwen reasoning across a tool loop (DashScope Responses) |
| alibaba-chat/qwen-3-8-max-obeys-named-tool-choice.json | Chat | a named tool_choice answered with that tool |
| alibaba-responses/qwen-3-8-max-uses-hosted-web-search-and-extraction.json | Responses | hosted (server-side) tool events mixed with text |
| meta-messages/replays-encrypted-thinking-through-a-tool-loop.json | Anthropic | encrypted thinking over a tool loop |
| meta-responses/replays-encrypted-reasoning-through-a-tool-loop.json | Responses | encrypted reasoning over a tool loop |
| venice-chat/gemini-preserves-per-call-signatures-through-parallel-tools.json | Chat | per-call signatures on parallel tool calls |
| venice-chat/claude-opus-4-8-preserves-reasoning-through-a-tool-loop-and-follow-up.json | Chat | Claude reasoning over an OpenAI-compatible gateway |
| openrouter-reasoning-tool-loop.json | Chat | OpenRouter reasoning fields over a tool loop |
| cloudflare-workers-ai/cloudflare-workers-ai-gpt-oss-20b-tools-tool-call.json | Chat | Workers AI gpt-oss tool call |
| deepinfra-chat/deepinfra-llama-3-3-70b-tool-loop.json | Chat | DeepInfra Llama tool loop |
| minimax-messages/m3-continues-a-tool-loop-with-adaptive-thinking.json | Anthropic | MiniMax M3 adaptive thinking over a tool loop |
| azure/responses-continues-after-a-tool-result.json | Responses | Azure OpenAI Responses continuation |
| azure/messages-calls-a-tool.json | Anthropic | Azure-hosted Anthropic Messages tool call |
| digitalocean-chat/haiku-reuses-a-cached-prompt.json | Chat | Anthropic-style cache_created_input_tokens / cache_read_input_tokens on a gateway |
| venice-chat/gemini-3-1-pro-preview-preserves-reasoning-through-a-tool-loop-and-follow-up.json | Chat | a `reasoning.text` and a `reasoning.encrypted` entry (with `data`, `id`) in reasoning_details; both go back |
| venice-chat/openai-gpt-54-mini-preserves-reasoning-through-a-tool-loop-and-follow-up.json | Chat | an encrypted-only reasoning_details entry (OpenAI Responses format) that goes back with its `data` |
