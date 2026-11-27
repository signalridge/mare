# 工具与 Shell 模式

mare 的全部内置工具都由 Lua 插件（`tools`、`tools-apply-patch`、`jobs`、`web`、`subagent`、`permissions`）注册，并经过同一条受保护管线执行：校验参数、检查策略、在操作系统沙箱内运行，并为内置文件工具（`write`、`edit`、`apply_patch`）在改写前保存文件快照（shell 子进程的文件副作用不纳入快照）。

## 内置工具

| 工具 | 所属插件 | 用途 |
|---|---|---|
| `read` | `tools` | 读取 UTF-8 文本文件（支持 `offset` / `limit`）、目录列表或图片（`png`、`jpg`、`jpeg`、`gif`、`webp`）。 |
| `write` | `tools` | 创建或覆盖文件，覆盖前自动记录快照。普通文件采用原子写入；目标已有多个硬链接时改为原地覆盖（`atomic = false`），以保留硬链接关系。 |
| `edit` | `tools` | 精确字符串替换（`old_string`、`new_string`、`replace_all`）。`edit_mode = "replace"` 时启用，或在 `"auto"` 下模型 `edit_tool` 与 provider 默认值均未选用 `apply_patch` 时回退启用。 |
| `apply_patch` | `tools-apply-patch` | 多文件补丁格式（`*** Begin Patch` / `*** End Patch`）。显式 `edit_mode` 优先；`edit_mode = "auto"` 时按模型的 `edit_tool` 或 provider 默认值选用（内置匹配覆盖 GPT 和 Codex），否则回退到 `replace`。 |
| `ls` | `tools` | 列出目录项，默认最多 `ls_limit`（500）条，包含点文件。 |
| `bash` | `tools` | 在 OS 沙箱内的持久 shell 或任务跟踪进程中执行命令。 |
| `powershell` | `tools` | Windows 下执行 PowerShell 命令（`powershell = "auto"` 默认开启，设 `"replace"` 可替代 `bash`）。 |
| `job_output`、`job_list`、`job_write`、`job_kill` | `jobs` | 读取、列出、向标准输入写入（或发 EOF），以及停止后台任务。 |
| `web_search`、`web_fetch` | `web` | 网页搜索或抓取 HTTP(S) 页面（内置 `web-fetch-http` 的 `http-text` 抓取后端，搜索可接 provider 原生或插件后端）。 |
| `tool_search` | `tools` | 按关键词或 `select:<name>` 检索延迟工具（如延迟暴露的 MCP 工具）并获取其输入 schema。 |
| `call_tool` | `tools` | 在不支持原生会话中追加工具（`tool_update = "addition_only"`）的模型上按名称与参数代理调用延迟工具，保持 `tools[]` 不变以保全提示词缓存。 |
| `subagent`、`subagent_*` | `subagent` | 创建与控制子代理（见[子代理与临时旁路对话](subagents.md)）。 |
| `request_permissions` | `permissions` | 附带理由申请额外读写路径或网络主机，支持单轮或本会话作用域（见[权限与沙箱](permissions.md#申请额外权限)）。 |

### 先读后改与编辑模式

`tools` 插件默认开启 `read_before_edit = true`：`edit`、`apply_patch` 和覆盖已有文件的 `write` 要求当前会话先读过该文件，并且若磁盘文件自读取后发生变化就会拒绝写入。

在 `init.lua` 中调整编辑模式与超时参数：

```lua
mare.plugin("tools", {
  edit_mode = "auto",        -- "auto" | "apply_patch" | "replace"
  read_before_edit = true,
  bash_timeout_ms = 120000,
  max_lines = 2000,
  max_bytes = 50 * 1024,
})
```

### 禁用工具或注册命令工具

可以直接在 `init.lua` 中禁用指定工具或把命令行程序包装成工具，也可以在启动时通过 `--exclude-tools` 按次排除工具（可重复传入或用逗号分隔，支持 `*` 通配符，如 `mare --exclude-tools bash,write` 或 `mare --exclude-tools "mcp__*"`）：

```lua
mare.tools.disable({ "ls" })

mare.tool.command {
  name = "grep",
  description = "Search file contents with ripgrep.",
  command = { "rg", "--line-number", "--no-heading", "--max-columns=500", "--regexp={pattern}", "--", "{path}" },
  params = {
    pattern = { type = "string", required = true },
    path = { type = "string", default = "." },
  },
  readonly = true,
}
```

`mare.tool.command` 注册的命令默认会清洗继承的环境变量（剥离名字包含 `KEY`、`PASSWORD`、`SECRET`、`TOKEN` 的变量以及继承的 `MARE_*`），并遵循 `tools` 插件的 `env` 配置：`env.include` 显式保留本会被清洗的继承变量，`env.exclude` 额外剔除指定变量，`env.set` 在清洗后赋值。此外，mare 自身也会向子进程注入运行时信息（`MARE_CWD`、`MARE_SESSION_ID`、`MARE_SESSION_MODEL`、`MARE_SESSION_EFFORT`，启用 peer 时还有 `MARE_PEER_SOCKET`）。未声明 `readonly = true` 时，mare 默认视其带有副作用。

## 联网搜索与抓取

`web` 与 `web-fetch-http` 插件提供 `web_search` 和 `web_fetch`。`web-fetch-http` 会拦截包含内嵌凭据的 URL 以及私有、回环、链路本地和云元数据 IP 地址（即便在 `auto` 预设下也是如此），外部网页返回的内容在送给模型前也会自动包裹一层“不可信数据”提示。

在会话内用 `/web` 查看或切换后端：

- `/web`：查看当前搜索模式（默认 `cached`）和各后端状态。
- `/web mode disabled|cached|indexed|live`：切换搜索模式。
- `/web search <id|auto>`、`/web fetch <id|auto>`：指定搜索或抓取后端（`http-text` 为内置抓取后端）。

```lua
mare.plugin("web", {
  web_search = "cached",
  search_timeout_ms = 30000,
  fetch_max_output_chars = 200000,
})
```

## `@file` 引用与图片附件

### `@file` 路径补全（`file-ref`）

在输入框输入 `@` 会触发工作区以及通过启动参数 `--add-dir <DIR>` 或会话命令 `/add-dir` 追加的目录的文件模糊补全（含空格的路径写成 `@"path with spaces"`）。`--add-dir` 可重复传入，仅接受已存在且不与宿主或项目受保护路径重叠的物理绝对目录（`mare acp` 请改用 ACP 的 `additionalDirectories`）。

- 默认限制：单文件上限 `100 KiB`（`max_bytes`），单条消息总上限 `256 KiB`（`max_total_bytes`）。
- 引用目录时会附加目录清单。
- 缺失、二进制、被策略拒绝或超限的文件不会悄悄丢弃，消息里会附一行说明原因并提示改用 `read` 工具。

### 图片附件（`images`）

在终端按 `Ctrl+V`（Windows 下为 `Alt+V`）从剪贴板粘贴图片，或让模型通过 `read` 读取图片文件。在支持 Kitty、iTerm2 或 sixel 协议的终端中，`images` 插件会把图片直接画在对话里；其他终端显示 `[image]`。

```lua
mare.plugin("images", {
  user = true,
  read = true,
  tools = true,
  max_rows = 12,
  max_cols = 60,
})
```

## 大输出转存（`spill`）

- Shell 命令日志：`bash` 输出超过 `shell_buffer_bytes`（`1 MiB`）或截断阈值（`2000` 行 / `50 KiB`）时，完整内容流式写入 `~/.local/state/mare/spill/bash-*.log`，工具结果返回截断文本与日志路径。已退出进程的旧日志自动清理。
- 会话私有 `spill` 服务：工具、渲染器和 RPC 宿主通过 `spill.save` / `spill.read` 保存私有文本或二进制对象（单对象上限 `8 MiB`），严格绑定 `session_id`，默认 `cleanup_period_days`（`30` 天）后清理。

## `!` Shell 模式与后台任务

### 交互式 Shell 模式（`shell-mode`）

在空输入框以 `!` 开头即可亲自运行命令，复用与 `bash` 工具相同的 shell、`prelude` 和 `MARE_*` 环境，并且跳过审批弹窗：

```text
!git status --short      # 运行命令；命令和输出加入模型上下文
!!cat .env.example       # 运行命令；输出只显示在终端，不进入上下文
```

命令运行中按 `Esc` 或 `Ctrl+C` 中止；空 shell 行按 `Backspace` 退出 shell 模式。进入上下文的输出会保留首尾、中间截断，上限 `2000` 行 / `50 KiB`。

### 后台任务（`jobs`）

耗时较长的命令可以在后台运行，不阻塞当前交互：

- 前台命令运行时按 `Ctrl+B` 把它转入后台。
- 模型可在 `bash` 里传 `run_in_background = true`（需要交互时加 `tty = true` 或 `stdin = true`），再用 `job_output`（支持 `until = "<正则>"` 或 `tail = N`）、`job_write` 和 `job_kill` 管理。
- 输入 `/jobs` 查看所有运行中和已结束的任务、查看末尾输出（`view_tail_bytes`）或停止任务。任务结束时，mare 会附带最多 `notify_inline_bytes`（2048 字节）输出通知代理。

```lua
mare.plugin("jobs", {
  max_running = 8,
  max_jobs = 64,
  kill_grace_ms = 3000,
})
```

## 相关页面

- [权限与沙箱](permissions.md)：操作系统沙箱与 `bash` 规则
- [MCP 服务器](mcp.md)：外部工具与 `tool_search` 延迟加载
- [基于输出的图像资源（英文）](https://github.com/signalridge/mare/blob/main/docs/image-resources.md)：artifact 图片缓存机制
