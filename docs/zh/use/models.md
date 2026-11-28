# 模型与 provider

如何登录 provider、选择模型和推理强度、覆盖模型元数据，以及添加自定义 provider。

模型以 `provider/id` 命名，例如 `openai/gpt-6-luna`。provider 是内置插件；每个 provider 注册自己的模型，有些在你登录后还会获取实时的模型列表。

## Provider

| Provider | 插件 | 认证方式 | 环境变量 |
| --- | --- | --- | --- |
| `openai` | `provider-openai` | ChatGPT 订阅（浏览器或设备码），或 API 密钥 | `OPENAI_API_KEY` |
| `anthropic` | `provider-anthropic` | API 密钥 | `ANTHROPIC_API_KEY` |
| `google` | `provider-google` | Gemini API 密钥 | `GEMINI_API_KEY`、`GOOGLE_API_KEY` |
| `opencode-go` | `provider-opencode-go` | opencode Go API 密钥 | `OPENCODE_API_KEY` |
| `deepseek` | `provider-openai-compat` | API 密钥 | `DEEPSEEK_API_KEY` |
| `kimi` | `provider-openai-compat` | Moonshot API 密钥 | `MOONSHOT_API_KEY` |
| `kimi-cn` | `provider-openai-compat` | Moonshot 中国区 API 密钥 | `MOONSHOT_API_KEY` |
| `kimi-code` | `provider-openai-compat` | Kimi Code API 密钥 | `KIMI_API_KEY` |
| `kimi-code-global` | `provider-openai-compat` | Kimi Code 国际区 API 密钥 | `KIMI_API_KEY` |
| `minimax` | `provider-openai-compat` | MiniMax API 密钥 | `MINIMAX_API_KEY` |
| `minimax-cn` | `provider-openai-compat` | MiniMax 中国区 API 密钥 | `MINIMAX_API_KEY` |
| `zai` | `provider-openai-compat` | Z.ai API 密钥 | `ZAI_API_KEY` |
| `zhipu-cn` | `provider-openai-compat` | 智谱 GLM 中国区 API 密钥 | `ZHIPU_API_KEY` |
| `openrouter` | `provider-openai-compat` | API 密钥 | `OPENROUTER_API_KEY` |
| `ollama` | `provider-openai-compat` | 无（`http://localhost:11434/v1`） | |
| `lmstudio` | `provider-openai-compat` | 无（`http://localhost:1234/v1`） | |

`provider-faux` 是用于测试和离线演示的脚本化 provider；不适合真实工作。

mare 按以下顺序查找密钥：`init.lua` 中用 `mare.provider` 设置的 `api_key`，然后是 `mare login` 保存的凭据（`~/.config/mare/auth.json`，权限 0600），最后是该 provider 的环境变量。

## 登录

```sh
mare login                              # 优先登录当前配置模型对应的 provider；无可用默认时 TTY 下打开选择器，无 TTY 退出码 2
mare login openai                       # 显式使用 ChatGPT 订阅（openai）登录
mare login openai --oauth               # 显式使用浏览器 OAuth 登录
mare login openai --device-auth         # ChatGPT 设备码登录（SSH 或无浏览器环境）
printenv ANTHROPIC_API_KEY | mare login anthropic --with-api-key
printenv OPENAI_API_KEY | mare login openai --with-api-key
mare login status                       # 查看全部 provider 及其凭据状态
mare logout openai                      # 清理本地凭据（仅对 openai-codex 尽力吊销 OAuth 令牌）
```

- 不带参数运行 `mare login` 时优先尝试当前模型对应的 provider；无可用默认项时在 TTY 下打开选择器，无 TTY 以退出码 `2` 退出（显式登录 ChatGPT 请用 `mare login openai`）。
- `--with-api-key` 仅从 stdin 读取密钥；stdin 为终端时拒绝执行，避免密钥写入 shell 历史。
- 会话内 `/login [provider]` 仅传入 provider 名称并在成功后后台刷新模型目录。
- 通过 `--rpc` 登录时，交互流程需要已连接的 UI；显式传入 API Key 请调用 `llm.login` 的 `key` 参数。
- 无显示环境（SSH，或未设置 `DISPLAY` / `WAYLAND_DISPLAY` 的 Linux）下，ChatGPT 登录优先提供设备码流程。

### Provider 登录契约与 `auth_keys`

通过 `ctx.llm:register_provider` 注册的自定义 provider 插件，可以声明以下字段来接入 `/login`、`mare login`、`mare login status` 和 `mare logout`：

- `login = { methods = { "oauth", "device", "api_key" }, run = function(io) ... end }`：声明支持的登录方式，mare 会在调用 `run(io)`（`io` 提供 `say`、`ask`、`choose`、`method` 以及可选的 `key`）之前校验 `--oauth` / `--device-auth` / `--with-api-key`。调用方未指定 `method` 时（例如 `/login <provider>`），`run(io)` 收到的 `io.method` 为 `nil`；只有显式传入 `method = "auto"` 时才会预先解析出具体登录方式。
- `auth_keys = { ... }`：列出该 provider 在 `auth.json` 中拥有的全部凭据键名（默认 `{ provider }`）。例如 `openai` 声明 `auth_keys = { "openai-codex", "openai" }`，`mare logout openai` 会将这两条本地凭据从 `auth.json` 中一并删除；其中远端令牌吊销仅对 `openai-codex` OAuth 令牌尽力尝试，独立粘贴保存的 OpenAI API Key 仅从本地移除，不会在 OpenAI 服务端自动吊销。

Lua 插件可以通过 `ctx.llm:auth_keys(prefix?)`（返回 `auth.json` 中按字典序排列的凭据键名数组，支持按字面前缀过滤）或 `ctx.llm:list_auth_keys(prefix?)`（返回 `{ data = string[] }`）列出已保存的凭据键名。RPC 侧的 `llm.list_auth_keys`（`{ prefix? } -> { data: string[] }`）、`llm.login` 和 `llm.logout` 属于 primary 管理员方法（仅限 owner/admin 连接调用，不对 peer 开放；`llm.list_auth_keys` 仅返回键名，不返回任何密钥值，也不会执行密钥命令），而 `llm.list_providers` 可按需授权给 peer 客户端。

非交互使用时，改为设置环境变量；不会向磁盘写入任何内容：

```sh
export ANTHROPIC_API_KEY=...
mare -p -m anthropic/claude-sonnet-5-5 "summarize the diff"
```

要从密码管理器读取密钥，可以给 `api_key` 一个命令。该命令按需运行，其输出会被缓存，直到辅助程序报告 `expires_at` 或请求返回 401：

```lua
mare.provider("anthropic", { api_key = { cmd = { "pass", "show", "anthropic/api-key" } } })
```

## 选择模型

```sh
mare -m openai/gpt-6-luna               # this run
mare -m sonnet                          # an alias
```

在会话中，`/model` 打开选择器。没有凭据的 provider 被归到末尾，标为需要登录。`/model <provider/id>` 或 `/model <alias>` 直接切换，`/model <text>` 打开按该文本过滤的选择器。打开选择器会在后台刷新超过十分钟的模型列表。`Alt+P` 打开选择器而不丢失你的草稿；`Alt+M` 在你收藏的模型之间循环，没有收藏时则在最近使用的模型之间循环。

要设置默认值，使用 `init.lua`：

```lua
mare.setup {
  model = "openai/gpt-6-luna",
  favorite_models = { "openai/gpt-6-luna", "anthropic/claude-sonnet-5-5" },
  fallback_models = { "google/gemini-2.5-pro" },   -- tried when the current model is overloaded or out of quota
}
```

你也可以设置 `MARE_MODEL`。`init.lua` 中显式指定的 `model`（或标志、变量）优先于你上次用 `/model` 选择的模型，后者由 mare 记在 `state.json` 中（见下文）。内置别名 `sonnet`、`opus` 和 `haiku` 选择该系列最新的 Anthropic 模型。可以用 `mare.model` 定义自己的别名：

```lua
mare.model { provider = "openai", id = "gpt-6-luna", alias = "main" }
```

之后 `-m main`、`/model main` 和 `fallback_models` 都接受 `main`。

## 推理强度

强度控制模型在回答之前推理的程度。级别有 `off`、`minimal`、`low`、`medium`、`high`、`xhigh` 和 `max`；每个模型只提供其中一部分。

```sh
mare -e high "refactor this module"
```

在会话中，`/effort`列出当前模型支持的级别。`/effort high` 为此模型设置一个级别并记住它。`/effort high --session` 只改变当前会话。`Alt+,` 降低级别，`Alt+.` 提高级别。如果你要求的级别模型不支持，mare 会使用最接近的受支持级别并告诉你；它绝不会悄悄收窄。`/show-reasoning` 切换显示完整的思考文本，而不是一行摘要。

没有强度参数的模型会报告这一点，并列出它确实具有的参数。

## 刷新模型列表

能够列出自身模型的 provider 会把列表缓存在 `~/.local/state/mare/models-cache/<provider>.json` 中，并在列表超过一天时于启动后在后台刷新。价格、输入类型和限制等元数据会从 models.dev 合并进来。立即刷新：

```sh
mare model refresh
```

这会获取每个有凭据的 provider 的模型列表，以及 models.dev 的元数据。把 `llm` 插件选项 `models_dev` 设为 `false` 可停止使用 models.dev：

```lua
mare.plugin("llm", { models_dev = false, catalog_refresh_ms = 7 * 24 * 3600 * 1000 })
```

## 按模型覆盖

`mare.setup` 中的 `models` 覆盖某个模型的字段，或定义 provider 没有列出的模型。键是 `provider/id` 引用。

```lua
mare.setup {
  models = {
    ["openai/gpt-6.1-terra"] = { context_window = 921000, max_output = 64000 },
    ["openrouter/qwen/qwen3-coder"] = { name = "Qwen3 Coder", context_window = 262144, reasoning = true },
  },
}
```

常用字段有 `name`、`context_window`、`max_output`、`reasoning`、`efforts`、`input`（列表，例如 `{ "text", "image" }`）和 `cost`（`input`、`output`、`cache_read`，每百万 token）。其他方面未知的模型，在你设置之前，上下文窗口为 128,000 个 token，最大输出为 16,384。上下文窗口很重要：[自动压缩](sessions.md)由它触发。

## 添加自定义 provider

任何支持 mare 内置协议线路（wire）的服务器都可以用 `mare.provider` 添加。不是内置 provider 的名称加上 `base_url`，就定义了一个新的 provider。同一服务的两个账号就是两个名称。

通过 `wire_api` 选择协议（默认 `"chat"`）：

| `wire_api` | 线路名称（`wire`） | 请求端点 | 原生认证请求头 |
| --- | --- | --- | --- |
| `"chat"`（默认） | `openai-chat` | `<base_url>/chat/completions` | `Authorization: Bearer <key>` |
| `"responses"` | `openai-responses` | `<base_url>/responses` | `Authorization: Bearer <key>` |
| `"anthropic"` | `anthropic-messages` | `<base_url>/v1/messages`（若 `base_url` 已以 `/v1` 结尾则为 `<base_url>/messages`） | `x-api-key: <key>` + `anthropic-version` |
| `"google"` | `google-generate` | `<base_url>/models/<model>:streamGenerateContent?alt=sse` | `x-goog-api-key: <key>` |

示例——OpenAI Chat Completions 端点（默认 `wire_api = "chat"`）：

```lua
mare.provider("lab", {
  base_url = "http://10.0.0.5:8080/v1",
  api_key = "LAB_API_KEY",            -- 环境变量名、{ cmd = {...} } 辅助命令，或字面量
  default_model = "qwen3",
  default_context = 131072,
  models = {
    { model_id = "qwen3", context_window = 131072, max_output = 8192, reasoning = true, efforts = { "low", "medium", "high" } },
  },
})
```

示例——Anthropic Messages 端点（`wire_api = "anthropic"`）：

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

四种协议通用的字段：`no_key = true` 用于无需认证的服务器，`list_models = false` 禁用自动拉取模型列表（`GET /models` 或 `GET /v1/models`），以及 `api_key`、`api_key_env`、`default_model`、`default_context`、`default_max_output`、`default_reasoning`、`key_prefixes`、`models_dev`（models.dev 的厂商 ID，或设为 `false` 禁用富化）、`headers`、`query_params`、`retries`、`stream_idle_timeout_ms`、`connect_timeout_ms`、`drop_fields`、`transport` 和 `proxy`。针对 `anthropic`（`anthropic-messages`）端点，还支持 provider 级的 `auth`（`"x-api-key"` 或 `"bearer"`）、`replay_unsigned_reasoning`、`fine_grained_tool_streaming`、`reasoning_budgets`、`reasoning_display`、`supports_long_cache_retention` 以及需显式开启的 `tool_update = "addition_only"`（自定义端点默认关闭）。针对 `chat`（`openai-chat`）端点，还可用 `dialect`（`"deepseek"`、`"openrouter"`、`"kimi"`、`"cerebras"`、`"zai"`、`"together"`、`"qwen"` 等）、`effort_param`（`"reasoning_effort"` 或 `"openrouter"`）、`reasoning_format`、`cache_control_format` 和 `needs_reasoning_placeholder` 按声明选择协议特性而不再依赖 provider 名称；更窄的显式字段优先于 `dialect`，其次才按主机名或模型 ID 探测。

### 预设叠加与区域端点

对已有内置名称调用 `mare.provider` 会逐字段叠加到预设之上（给出 `models` 时整体替换预设种子列表；在数据条目上改写 `wire` 或 `wire_api` 会将其迁到对应线路，而代码型 provider 会忽略配置里的 `wire`/`wire_api` 并在 `mare doctor` 中报告警告）。内置预设默认使用国际端点；若需切换到大陆端点或内部网关，只需覆盖 `base_url`，即可保留其方言与环境变量设置：

```lua
mare.provider("kimi", { base_url = "https://api.moonshot.cn/v1" })
mare.provider("anthropic", { base_url = "https://gateway.example.com", headers = { ["x-team"] = "infra" } })
```

### 自定义端点与 provider 插件

与现有 wire 兼容的服务可通过 `mare.provider` 直接配置。需要自定义认证、签名或请求格式时，可编写 provider 插件。

## `state.json` 中的运行时选择

当你在会话内选择模型或强度时，mare 会把它保存到 `~/.local/state/mare/state.json`，而不是编辑 `init.lua`。该文件保存 `model`、`recent_models` 和 `model_params`（为每个模型记住的强度），以及状态栏片段等其他选择。下次运行会从这些值开始，除非 `init.lua`、环境变量或标志设置了同一内容。删除对应的键，或用 `--clean` 运行，即可忽略它。`/config` 显示每个选项的值及其来源。
