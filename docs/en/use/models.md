# Models and providers

How to sign in to a provider, pick a model and a reasoning effort, override model metadata, and add a custom provider.

A model is named `provider/id`, for example `openai/gpt-6-luna`. The providers are built-in plugins; each registers its models, and some also fetch a live model list once you are signed in.

## Providers

| Provider | Plugin | Authentication | Environment variable |
| --- | --- | --- | --- |
| `openai` | `provider-openai` | ChatGPT subscription (browser or device code), or an API key | `OPENAI_API_KEY` |
| `anthropic` | `provider-anthropic` | API key | `ANTHROPIC_API_KEY` |
| `google` | `provider-google` | Gemini API key | `GEMINI_API_KEY`, `GOOGLE_API_KEY` |
| `opencode-go` | `provider-opencode-go` | opencode Go API key | `OPENCODE_API_KEY` |
| `deepseek` | `provider-openai-compat` | API key | `DEEPSEEK_API_KEY` |
| `kimi` | `provider-openai-compat` | Moonshot API key | `MOONSHOT_API_KEY` |
| `kimi-cn` | `provider-openai-compat` | Moonshot China API key | `MOONSHOT_API_KEY` |
| `kimi-code` | `provider-openai-compat` | Kimi Code API key | `KIMI_API_KEY` |
| `kimi-code-global` | `provider-openai-compat` | Kimi Code Global API key | `KIMI_API_KEY` |
| `minimax` | `provider-openai-compat` | MiniMax API key | `MINIMAX_API_KEY` |
| `minimax-cn` | `provider-openai-compat` | MiniMax China API key | `MINIMAX_API_KEY` |
| `zai` | `provider-openai-compat` | Z.ai API key | `ZAI_API_KEY` |
| `zhipu-cn` | `provider-openai-compat` | Zhipu GLM China API key | `ZHIPU_API_KEY` |
| `openrouter` | `provider-openai-compat` | API key | `OPENROUTER_API_KEY` |
| `ollama` | `provider-openai-compat` | none (`http://localhost:11434/v1`) | |
| `lmstudio` | `provider-openai-compat` | none (`http://localhost:1234/v1`) | |

`provider-faux` is a scripted provider for tests and offline demos; it is not meant for real work.

mare looks for a key in this order: the `api_key` set with `mare.provider` in `init.lua`, then the credential stored by `mare login` (`~/.config/mare/auth.json`, mode 0600), then the provider's environment variables.

## Signing in

```sh
mare login                              # configured model's provider; picker on TTY (exits 2 without TTY)
mare login openai                       # ChatGPT subscription (openai) explicitly
mare login openai --oauth               # force browser OAuth explicitly
mare login openai --device-auth         # ChatGPT with a device code, for SSH or no browser
printenv ANTHROPIC_API_KEY | mare login anthropic --with-api-key
printenv OPENAI_API_KEY | mare login openai --with-api-key
mare login status                       # every provider and whether it has credentials
mare logout openai                      # remove local stored credentials (best-effort OAuth revocation for openai-codex only)
```

- Bare `mare login` tries the configured model's provider first. With no usable default, it opens a provider picker on a TTY and exits `2` without one (pass `mare login openai` explicitly for ChatGPT).
- `--with-api-key` reads from stdin only and refuses to run when stdin is a TTY, keeping keys out of shell history.
- Inside a session, `/login [provider]` passes the provider name to `llm` and refreshes that provider's model catalog in the background on success.
- Over `--rpc`, interactive login requires a connected UI; pass an API key explicitly via the `key` parameter of `llm.login`.
- Without a display (SSH, or Linux without `DISPLAY` / `WAYLAND_DISPLAY`), ChatGPT login offers device-code flow first.

### Provider login contract and `auth_keys`

Custom provider plugins registered with `ctx.llm:register_provider` participate in `/login`, `mare login`, `mare login status`, and `mare logout` by declaring:

- `login = { methods = { "oauth", "device", "api_key" }, run = function(io) ... end }`: advertises the supported login methods so mare validates `--oauth` / `--device-auth` / `--with-api-key` before calling `run(io)` (where `io` provides `say`, `ask`, `choose`, `method`, and optional `key`). When a caller omits `method` (such as `/login <provider>`), `run(io)` receives `io.method == nil`; passing `method = "auto"` resolves a concrete method before invoking `run(io)`.
- `auth_keys = { ... }`: lists every key in `auth.json` owned by the provider (defaulting to `{ provider }`). For example, `openai` declares `auth_keys = { "openai-codex", "openai" }`, so `mare logout openai` removes both local entries from `auth.json`. Its server-side revocation runs best-effort only for the `openai-codex` OAuth token; a separately pasted OpenAI API key is removed locally and is not revoked at OpenAI.

Lua plugins can inspect stored credential key names without reading secret values or running key commands via `ctx.llm:auth_keys(prefix?)` (returns sorted key names from `auth.json`, optionally filtered by literal prefix) or `ctx.llm:list_auth_keys(prefix?)` (returns `{ data = string[] }`). Over RPC, `llm.list_auth_keys` (`{ prefix? } -> { data: string[] }`), `llm.login`, and `llm.logout` are primary admin methods (restricted to owner/admin connections; `llm.list_auth_keys` returns key names only and never exposes secret values), while `llm.list_providers` can also be granted to peer clients.

For non-interactive use, set the environment variable instead; nothing is written to disk:

```sh
export ANTHROPIC_API_KEY=...
mare -p -m anthropic/claude-sonnet-5-5 "summarize the diff"
```

To read a key from a password manager, give `api_key` a command. The command runs on demand, and its output is cached until the helper reports `expires_at` or a request returns 401:

```lua
mare.provider("anthropic", { api_key = { cmd = { "pass", "show", "anthropic/api-key" } } })
```

## Choosing a model

```sh
mare -m openai/gpt-6-luna               # this run
mare -m sonnet                          # an alias
```

In a session, `/model` opens the picker. Providers without credentials are grouped at the end as needing login. `/model <provider/id>` or `/model <alias>` switches directly, and `/model <text>` opens the picker filtered by that text. Opening the picker refreshes model lists older than ten minutes in the background. `Alt+P` opens the picker without losing your draft; `Alt+M` cycles through your favorite models, or the recently used ones if you have no favorites.

To set a default, use `init.lua`:

```lua
mare.setup {
  model = "openai/gpt-6-luna",
  favorite_models = { "openai/gpt-6-luna", "anthropic/claude-sonnet-5-5" },
  fallback_models = { "google/gemini-2.5-pro" },   -- tried when the current model is overloaded or out of quota
}
```

You can also set `MARE_MODEL`. An explicit `model` in `init.lua` (or the flag or variable) wins over the model you last picked with `/model`, which mare remembers in `state.json` (see below). The built-in aliases `sonnet`, `opus` and `haiku` select the newest Anthropic model of that family. Define your own with `mare.model`:

```lua
mare.model { provider = "openai", id = "gpt-6-luna", alias = "main" }
```

After that `-m main`, `/model main` and `fallback_models` accept `main`.

## Reasoning effort

Effort controls how much the model reasons before it answers. The levels are `off`, `minimal`, `low`, `medium`, `high`, `xhigh` and `max`; each model offers only some of them.

```sh
mare -e high "refactor this module"
```

In a session, `/effort` lists the levels the current model supports. `/effort high` sets one for this model and remembers it. `/effort high --session` changes only the current session. `Alt+,` lowers and `Alt+.` raises the level. If you ask for a level the model does not support, mare uses the nearest supported one and says so; it never clamps silently. `/show-reasoning` toggles showing full reasoning text instead of a one-line summary.

A model without an effort parameter reports that and lists the parameters it does have.

## Refreshing the model list

Providers that can list their models cache the list in `~/.local/state/mare/models-cache/<provider>.json` and refresh it in the background after startup when it is older than a day. Metadata such as price, input types and limits is merged in from models.dev. To refresh now:

```sh
mare model refresh
```

This fetches the model lists of every provider that has credentials, plus the models.dev metadata. Set the `llm` plugin option `models_dev = false` to stop using models.dev:

```lua
mare.plugin("llm", { models_dev = false, catalog_refresh_ms = 7 * 24 * 3600 * 1000 })
```

## Per-model overrides

`models` in `mare.setup` overrides fields of a model, or defines one the provider does not list. The key is the `provider/id` ref.

```lua
mare.setup {
  models = {
    ["openai/gpt-6.1-terra"] = { context_window = 921000, max_output = 64000 },
    ["openrouter/qwen/qwen3-coder"] = { name = "Qwen3 Coder", context_window = 262144, reasoning = true },
  },
}
```

Common fields are `name`, `context_window`, `max_output`, `reasoning`, `efforts`, `input` (a list such as `{ "text", "image" }`) and `cost` (`input`, `output`, `cache_read`, per million tokens). A model that is not otherwise known gets a 128,000-token context window and 16,384 maximum output until you set them. The context window matters: [automatic compaction](sessions.md) triggers from it.

## Adding a custom provider

Any server that speaks one of mare's built-in wire protocols can be added with `mare.provider`. A name that is not a built-in provider, plus a `base_url`, defines a new provider. Two accounts of one service are two names.

Select the protocol with `wire_api` (defaults to `"chat"`):

| `wire_api` | Wire name (`wire`) | Request endpoint | Native authentication header |
| --- | --- | --- | --- |
| `"chat"` (default) | `openai-chat` | `<base_url>/chat/completions` | `Authorization: Bearer <key>` |
| `"responses"` | `openai-responses` | `<base_url>/responses` | `Authorization: Bearer <key>` |
| `"anthropic"` | `anthropic-messages` | `<base_url>/v1/messages` (or `<base_url>/messages` if `base_url` already ends in `/v1`) | `x-api-key: <key>` + `anthropic-version` |
| `"google"` | `google-generate` | `<base_url>/models/<model>:streamGenerateContent?alt=sse` | `x-goog-api-key: <key>` |

Example — an OpenAI Chat Completions endpoint (`wire_api = "chat"`, the default):

```lua
mare.provider("lab", {
  base_url = "http://10.0.0.5:8080/v1",
  api_key = "LAB_API_KEY",            -- an environment variable name, a { cmd = {...} } helper, or a literal
  default_model = "qwen3",
  default_context = 131072,
  models = {
    { model_id = "qwen3", context_window = 131072, max_output = 8192, reasoning = true, efforts = { "low", "medium", "high" } },
  },
})
```

Example — an Anthropic Messages endpoint (`wire_api = "anthropic"`):

```lua
mare.provider("claude-proxy", {
  wire_api = "anthropic",
  base_url = "https://gateway.example.com/anthropic",
  api_key = "CLAUDE_PROXY_API_KEY",
  headers = { ["x-team"] = "infra" },
  default_model = "claude-sonnet-5-5",
  models = {
    { model_id = "claude-sonnet-5-5", context_window = 200000, max_output = 32000, reasoning = true, adaptive = true },
  },
})
```

```sh
mare -m lab/qwen3
mare -m claude-proxy/claude-sonnet-5-5
```

Common fields across all four wires: `no_key = true` for a server without authentication, `list_models = false` to disable catalog fetches (`GET /models` or `GET /v1/models`), `api_key` / `api_key_env`, `default_model`, `default_context`, `default_max_output`, `default_reasoning`, `key_prefixes`, `models_dev` (a models.dev provider id, or `false` to disable enrichment), `headers`, `query_params`, `retries`, `stream_idle_timeout_ms`, `connect_timeout_ms`, `drop_fields`, `transport` and `proxy`. For `anthropic` (`anthropic-messages`) endpoints, provider-level `auth` (`"x-api-key"` or `"bearer"`), `replay_unsigned_reasoning`, `fine_grained_tool_streaming`, `reasoning_budgets`, `reasoning_display`, `supports_long_cache_retention` and explicit `tool_update = "addition_only"` (off by default on custom endpoints) are also supported. For `chat` (`openai-chat`) endpoints, `dialect` (`"deepseek"`, `"openrouter"`, `"kimi"`, `"cerebras"`, `"zai"`, `"together"`, `"qwen"`, …), `effort_param` (`"reasoning_effort"` or `"openrouter"`), `reasoning_format`, `cache_control_format` and `needs_reasoning_placeholder` select protocol quirks without tying behaviour to the provider name; narrower fields take precedence over `dialect`, which in turn takes precedence over host and model-id detection.

### Overlays and regional endpoints

Calling `mare.provider` with an existing built-in name overlays that preset field by field (`models`, when given, replaces the preset's seed list; changing `wire` or `wire_api` moves a data entry to the new wire, while code-backed providers ignore `wire`/`wire_api` and report a `mare doctor` warning). Built-in presets default to international endpoints; override `base_url` to point a preset at a regional or mainland endpoint or an internal gateway while keeping its dialect and credential variable:

```lua
mare.provider("kimi", { base_url = "https://api.moonshot.cn/v1" })
mare.provider("anthropic", { base_url = "https://gateway.example.com", headers = { ["x-team"] = "infra" } })
```

### Custom endpoints and provider plugins

Services compatible with an existing wire can be configured directly with `mare.provider`. When a service requires custom authentication, signing, or request framing, write a provider plugin.

## Runtime choices in `state.json`

When you pick a model or an effort inside a session, mare saves it to `~/.local/state/mare/state.json` instead of editing `init.lua`. The file holds `model`, `recent_models` and `model_params` (the effort remembered for each model), plus other choices such as the status line segments. The next run starts from them, unless `init.lua`, an environment variable or a flag sets the same thing. Delete the key, or run with `--clean`, to ignore it. `/config` shows each option's value and where it came from.
