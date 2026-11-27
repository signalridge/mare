# ACP 服务器

`mare acp` 通过 stdio 把 mare 作为 Agent Client Protocol 服务器运行，这样支持 ACP 的编辑器就可以启动它，并在面板中与它对话。本页说明如何注册 mare、服务器实现了什么，以及它在哪些地方没有覆盖完整协议。

## 在编辑器中注册 mare

ACP 客户端把智能体作为子进程启动，并通过它的标准输入和标准输出以 JSON-RPC 通信。每个客户端都需要同样的两样东西：一个命令及其参数：

```json
{
  "command": "mare",
  "args": ["acp"]
}
```

这段配置放在哪里取决于编辑器。找到它用于自定义或外部 ACP 智能体的设置，把 `mare` 填为命令，把 `acp` 填为参数。请确保 `mare` 在编辑器能看到的 `PATH` 中；如果不在，请给出完整路径。

`mare acp` 在标准输入上等待协议消息，在收到之前不会打印任何内容。`mare acp --help` 列出它的选项。

## 选项

`mare acp` 接受启动选项的一个子集。把它们加到 `args` 中：

| 选项 | 效果 |
|---|---|
| `-m`、`--model <MODEL>` | 模型，例如 `openai/gpt-6.1-terra`。 |
| `--agent <NAME>` | 为初始会话使用指定的已注册智能体类型。 |
| `-e`、`--effort <EFFORT>` | 推理强度。 |
| `--tools <TOOLS>` | 只启用这些工具（逗号分隔）。 |
| `--exclude-tools <TOOLS>` | 排除这些工具（可重复或逗号分隔，支持 `*`）。 |
| `--append-system-prompt <TEXT>` | 在系统提示词后追加文本。 |
| `--name <NAME>` | 为会话命名。 |
| `--set <KEY=VALUE>` | 覆盖一个配置值，例如 `--set plugins.permissions.preset=read-only`。 |
| `--strict-config` | 出现配置警告时拒绝启动。 |
| `--trust`、`--no-trust` | 为本次运行覆盖已保存的项目信任决定（不落盘）。 |
| `--approve-mcps` | 无需询问即可批准项目及 `$MARE_MCP_CONFIG` 中的 MCP 服务器。 |
| `--clean` | 不使用用户或项目配置、用户插件和包。 |
| `--startuptime <FILE>` | 将启动耗时记录写入文件。 |
| `-C`、`--cwd <CWD>` | 默认工作目录。 |

例如，固定使用某个模型的只读服务器：

```json
{ "command": "mare", "args": ["acp", "--model", "openai/gpt-6.1-terra", "--set", "plugins.permissions.preset=read-only"] }
```

会话标志（`-c`、`-r`、`--no-session`）和 `--permission` 不是 `mare acp` 的选项。请改用 `--set plugins.permissions.preset=...`；取值为 `read-only`、`auto` 和 `full-access`（见[权限](../use/permissions.md)）。

## 已实现的内容

服务器实现以下消息：

| 消息 | 类型 | 行为 |
|---|---|---|
| `initialize` | 请求 | 返回客户端请求版本与服务端 `ProtocolVersion::LATEST` 中的较低值；声明 HTTP MCP、图片和嵌入上下文提示词、`loadSession`，以及会话列表、恢复和关闭能力。 |
| `authenticate` | 请求 | 服务端不要求认证，收到 `authenticate` 时返回空成功响应。 |
| `session/new` | 请求 | 在客户端发送的 `cwd` 中启动一个新的 mare 会话并返回其 id。 |
| `session/load` | 请求 | 在原来的 `cwd` 中重新打开已保存且未活动的会话，并在返回前通过 `session/update` 回放历史；对于已压缩的会话，仅回放压缩后的上下文。 |
| `session/resume` | 请求 | 在原来的 `cwd` 中重新打开已保存且未活动的会话。 |
| `session/list` | 请求 | 列出已保存且未活动的会话，可按 `cwd` 筛选。 |
| `session/close` | 请求 | 关闭活动会话。 |
| `session/set_config_option` | 请求 | 更改会话的模型或推理强度。 |
| `session/prompt` | 请求 | 运行一轮，结束时返回停止原因。 |
| `session/cancel` | 通知 | 中止该会话正在运行的一轮。 |
| `session/request_permission` | 服务器发出的请求 | 请客户端批准工具或网络操作。选项包括 `allow-once`、`reject-once`，以及仅在提供时可选的 `allow-always`。 |

每个 `session/new` 都会为给定的 `cwd` 创建单独的 mare 宿主，所以不同文件夹中的多个会话可以并行运行。项目信任按各会话工作区判定：ACP 会优先尊重显式传入的 `--trust` 和 `--no-trust`，其次再读取 `trust.json` 中保存的决定或 `MARE_TRUST_DEFAULT`（见[权限](../use/permissions.md)）。ACP 从不交互式询问项目信任。在文件夹受信任之前，它的项目插件、技能和提示词都不会加载。可以在该文件夹运行一次 `mare trust` 记录决定，或显式传入 `--trust` / `--no-trust`。

`session/new`、`session/load` 或 `session/resume` 中提供的 `mcpServers` 接受 HTTP(S) 和 stdio 传输；stdio 命令必须使用绝对路径。不支持 SSE 传输。mare 自身的 MCP 配置见 [MCP](../use/mcp.md)。

### 提示词

提示词按原有顺序接受文本块、嵌入的文本资源、资源链接和图片。资源链接以文本表示，相邻文本用空行连接。音频和嵌入的二进制资源会被拒绝。

如果会话当前模型未声明支持图片输入，包含图片块的提示词会以参数无效错误被拒绝。模型按会话选择且可以更改，且 mare 在初始化时尚无会话宿主，无法按单个模型决定该标志，因此 `initialize` 为连接声明图片支持，并在每次包含图片的提示中检查当前模型。

一轮以下列停止原因之一结束：

| 停止原因 | 时机 |
|---|---|
| `end_turn` | 这一轮已完成。 |
| `cancelled` | 被 `session/cancel` 中止。 |
| `max_tokens` | 最后一条助手消息因长度而停止。 |
| `max_turn_requests` | 达到 `agent.max_turns` 上限。 |
| `refusal` | provider 拒绝或过滤了回复。 |

provider 错误和持久化失败会作为 JSON-RPC 内部错误返回，绝不会作为成功的 `end_turn`。未知的会话 id 会返回 JSON-RPC 错误 `-32002`（Resource not found）。

### 流式更新

一轮运行期间，mare 发送 `session/update` 通知：

| ACP 更新 | 来源 |
|---|---|
| `agent_message_chunk` | 助手文本的增量。 |
| `agent_thought_chunk` | 推理的增量。 |
| `tool_call` | 工具调用开始。`status` 为 `in_progress`，`title` 是工具名称加上它的 `path` 或 `command` 参数，`rawInput` 是参数。 |
| `tool_call_update` | 工具调用结束。`status` 为 `completed` 或 `failed`，内容是工具的文本输出。 |
| `usage_update` | 收到含 `last_context` 和正 `context_window` 的 `usage/changed` 时发送，包含 `used` 和 `size`。 |

工具调用的 `kind` 由内核工具元数据推导（声明 `command_tool` 时为 `execute`，按 `side_effect` 映射为 `read` / `edit`，其余情况及历史回放缺少元数据时为 `other`）。

## 局限

- **没有会话模式。** 服务器不声明 ACP 会话模式，也不实现 `session/set_mode`。
- **仅 `session/new` 支持额外目录。** `session/new` 支持 `additionalDirectories`（`cwd` 之外实际存在的物理目录，在恢复会话时重新校验）；`session/load` 和 `session/resume` 仍会拒绝非空的 `additionalDirectories`。
- **没有音频提示词。** 音频块和嵌入的二进制资源会被拒绝。
- **没有客户端的文件或终端方法。** mare 自己读写文件并运行命令。它不会调用编辑器的 `fs/*` 或 `terminal/*` 方法。
- **不要求认证。** `initialize` 不返回认证方式，收到 `authenticate` 时返回空成功响应。

如果需要带审批、宿主工具和完整会话 API 的程序化控制，请使用 [RPC](rpc.md) 接口。
