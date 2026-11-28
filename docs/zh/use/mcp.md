# MCP 服务器

把 Model Context Protocol 服务器连接到 mare：在哪里配置、工具如何到达模型、项目服务器如何获得批准，以及 OAuth 登录。

`mcp` 插件是内置的，默认开启。在你配置服务器之前它什么都不做。它支持 stdio 服务器（由 mare 启动的命令）和 streamable HTTP 服务器（一个 URL）。不支持旧的 SSE 传输；请使用服务器的 streamable HTTP 端点，通常是 `/mcp` 而不是 `/sse`。

## 配置服务器

服务器可以来自五个地方。对于同名服务器，后面的来源覆盖前面的。

| 顺序 | 来源 | 说明 |
| --- | --- | --- |
| 1 | `init.lua` 中 `mare.plugin("mcp", { ... })` 的 `servers` | |
| 2 | `~/.config/mare/mcp.json` | 用户文件。 |
| 3 | `$MARE_MCP_CONFIG` 指定的文件 | 需要批准（见下文）。 |
| 4 | 项目资源根中的 `.mcp.json` | 项目文件（向上直到 git 根最近的含 `.mare/` 或 `.mcp.json` 的目录，可能位于当前工作目录的上级）；需要批准。 |
| 5 | 项目资源根中的 `.mare/mcp.json` | 同一项目资源根下的项目文件；需要批准。 |

JSON 文件使用通用布局 `{ "mcpServers": { ... } }`（也会读取 `mcp_servers` 和 `servers`），并接受其他客户端使用的驼峰写法（`enabledTools`、`bearerTokenEnvVar`、`startupTimeoutMs` 等）。

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "${GITHUB_TOKEN}" }
    },
    "docs": {
      "url": "https://example.com/mcp",
      "headers": { "x-team": "platform" }
    }
  }
}
```

在 `init.lua` 中的等价写法：

```lua
mare.plugin("mcp", {
  servers = {
    github = {
      command = "npx", args = { "-y", "@modelcontextprotocol/server-github" },
      env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" },
      tier = "direct",
    },
    linear = {
      url = "https://mcp.linear.app/mcp", oauth = true,
      tools = { delete_issue = "hidden" },
    },
  },
})
```

服务器名称可以包含字母、数字、`_` 和 `-`。字符串中会展开 `${VAR}`、`${env:VAR}` 和 `${VAR:-default}`；未设置的变量会由 `/mcp` 和 `mare doctor` 报告。

没有 `command`、`url` 或 `type` 的项目条目，是对用户同名服务器的覆盖：它只能修改 `enabled`、`tier`、`tools`、`enabled_tools`、`disabled_tools`、`description` 和 `required`。

### 服务器键

| 键 | 含义 |
| --- | --- |
| `command`、`args`、`env`、`env_file`、`cwd`、`env_include`、`env_exclude` | stdio 服务器。`env_file` 是 dotenv 文件；`env` 会覆盖它。 |
| `url`、`headers`、`env_http_headers`、`bearer_token`、`bearer_token_env_var` | streamable HTTP 服务器。`env_http_headers` 把请求头名称映射到环境变量。 |
| `oauth` | `true`，或包含 `scopes`、`client_id`、`client_secret`、`client_name`、`callback_port`、`auth_server_metadata_url` 的表。 |
| `tls` | `{ ca_bundle = "..." }`，用于私有证书颁发机构。 |
| `enabled` | `false` 关闭该服务器。 |
| `required` | `true`：请求会等待此服务器连接，没有时间限制；连接失败时请求被拒绝并给出原因。 |
| `tier` | 暴露级别；见下文。 |
| `tools` | 按工具设置的级别，例如 `{ delete_issue = "hidden", ["search_*"] = "direct" }`。 |
| `enabled_tools`、`disabled_tools` | 允许列表或拒绝列表，支持 `*` 模式。 |
| `description` | 展示给模型，帮助它找到该服务器的工具。 |
| `startup_timeout_ms`、`call_timeout_ms` | 默认 60,000（最多 300,000）和 120,000。进度通知会重置调用超时。 |

MCP 插件选项有 `tier`（默认 `deferred`）、`startup_timeout_ms`、`call_timeout_ms` 和 `text_limit`。可通过 `plugins.tools.search_results` 修改 `tool_search` 的默认结果数量（5）。超过 `text_limit`（20 KiB）的结果会从中间截断，完整文本保存到一个私有文件，结果中会写明该文件名。

## 批准项目服务器

来自 `$MARE_MCP_CONFIG`、`.mcp.json` 或 `.mare/mcp.json` 的服务器在你批准之前不会启动，因为打开一个仓库不应该运行其中的命令。批准结果按服务器名称加一个哈希记录在 `~/.local/state/mare/mcp-approvals.json` 中。该哈希覆盖执行、鉴权、传输与暴露配置：`command`、`args`、`env`、`env_file`、`env_include`、`env_exclude`、`cwd`、`url`、`headers`、`env_http_headers`、`bearer_token`、`bearer_token_env_var`、`oauth`、`tls`、`type`、`tier`、`tools`、`enabled_tools`、`disabled_tools`、`enabled`、`required`、`description` 以及任何项目覆盖层。修改其中任意字段都会改变哈希并触发重新审批。来自项目的 stdio 服务器还要求该项目已被[信任](permissions.md)。

- 交互式：`/mcp enable <server>`，或在 `/mcp` 中选择该服务器并选择 Enable。
- 无界面：传入 `--approve-mcps` 可批准全部。这会把同样的批准结果写入 `mcp-approvals.json`，所以之后的运行（无界面或交互式）会直接启动这些服务器而不再询问，直到哈希改变：

```sh
mare -p --trust --approve-mcps "list my open issues"
```

没有它时，未批准的服务器显示为 `needs_approval`，其工具不可用。

## 工具如何到达模型

每个工具注册为 `mcp__<server>__<tool>`。如果名称长于 64 个字符或与另一个工具冲突，会追加一个短哈希。模型一开始能看到多少工具取决于服务器的级别，这样在服务器很多时可以保持提示词较小。

| 级别 | 行为 |
| --- | --- |
| `deferred`（默认） | 启动时不连接服务器。模型用 `tool_search` 查找工具及其参数 schema。没有缓存的服务器会在首次搜索时连接以发现工具；`~/.local/state/mare/mcp-tools` 中有缓存时则到首次使用才连接。支持 `tool_update = "addition_only"` 的模型（Anthropic Claude、OpenAI Responses `gpt-5.4` 及以上、Responses API 上的 DeepSeek V4.1 Flash 以及 Kimi K3）会在会话途中原生激活延迟工具并直接调用；其他模型则通过固定的 `call_tool` 代理工具调用，使顶层 `tools` 数组和提示词前缀缓存在各轮之间保持逐字节稳定。 |
| `direct` | 启动时在后台连接，工具像内置工具一样被声明。请求从不等待它（除非设置了 `required`）：仍在连接的工具从下一轮起加入。 |
| `hidden` | 不可用。 |
| `codemode` | 只能从代码模式脚本中调用。需要 codemode 插件；否则按 `deferred` 处理。 |

`tool_search` 接受关键词（`github issues`）或确切名称（`select:mcp__github__create_issue`，单次最多 32 个），并在固定结果预算内返回匹配工具的 schema。系统提示词中每个服务器占一行。

> **全提供商的提示词缓存：** 延迟工具发现机制在所有提供商上都保持系统提示词和顶层 `tools` 数组跨轮次不变。支持 `tool_update = "addition_only"` 的模型（Anthropic Claude、OpenAI Responses `gpt-5.4`+、DeepSeek V4.1 Flash、Kimi K3）使用原生会话中追加；其他端点（如 DeepSeek V4 Pro、GLM、Google Gemini、较早的 OpenAI 模型和 Kimi K2）通过固定的 `call_tool` 代理调用，不改动 `tools[]`，从而保全提示词前缀缓存。如果希望少量常用工具一开始就直接声明，可设置 `tier = "direct"`（或在 `tools` 中把个别工具设为 `"direct"`）。

如果服务器提供资源，模型还会得到列出和读取它们的工具，服务器的提示会显示为名为 `/mcp__<server>__<prompt>` 的斜杠命令（传入 `name=value` 参数或位置参数）。服务器的 elicitation 请求在 TUI 中显示为表单；无界面运行总是拒绝它们。

### MCP 工具的审批

MCP 工具运行在服务器的进程中，在 mare 的操作系统沙箱之外。因此在 `auto` 预设下，每次 MCP 工具调用都会先询问，包括只读的调用。要对你信任的工具跳过询问，添加一条规则：

```lua
mare.plugin("permissions", {
  rules = { { tool = "mcp__github__list_issues", decision = "allow" } },
})
```

或者在对话框中选择“Allow and remember”。见[权限](permissions.md)。

## 管理服务器

```sh
mare mcp list                 # connect to every enabled server; print state, tools and errors (exit 1 on required server problems)
mare mcp list-tools github    # a server's tools with their input schemas
mare mcp disable github       # turn a server off in this project
mare mcp enable github
mare mcp login linear         # OAuth login in the browser
```

`enable` 和 `disable` 按项目记录在 `~/.local/state/mare/mcp-disabled.json` 中；它们从不修改你的配置文件。

在会话内，`/mcp` 打开服务器面板，显示它们的状态（`connected`、`needs_login`、`needs_approval`、`disabled` 等）以及适合每个服务器的操作：显示工具和 schema、重新连接、登录或登出、启用或禁用。同样的操作也可以作为子命令使用：`/mcp login <server>`、`/mcp logout <server>`、`/mcp reconnect <server>`、`/mcp enable <server>`、`/mcp disable <server>` 和 `/mcp tools <server>`。`mare doctor` 也会检查 MCP 服务器。

## OAuth

在 HTTP 服务器上设置 `oauth = true`（或一个选项表），然后登录一次：

```sh
mare mcp login linear
```

mare 会打开浏览器，完成 OAuth 流程，保存令牌，并用它们重新连接服务器。需要登录的服务器在你登录之前显示为 `needs_login`。`/mcp logout <server>` 会忘记已保存的令牌。`login` 只适用于配置了 `oauth` 的 HTTP 服务器。

mare 从服务器的受保护资源元数据找到授权服务器，或者读取服务器同源的授权服务器元数据。以下情况会拒绝登录：元数据在另一个源上；服务器没有发布任何元数据（这时设置 `auth_server_metadata_url`）；授权服务器没有声明 PKCE `S256`；授权、令牌、注册或撤销端点在回环地址以外的主机上使用明文 `http`。

对于使用静态令牌的服务器，请使用 `bearer_token_env_var`，这样令牌就不会出现在文件里：

```json
{ "mcpServers": { "internal": { "url": "https://mcp.example.com/mcp", "bearer_token_env_var": "INTERNAL_MCP_TOKEN" } } }
```
