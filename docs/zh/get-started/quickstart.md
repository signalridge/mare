# 快速上手

登录 provider、在项目中启动 `mare`、批准工具调用，并编写 `~/.config/mare/init.lua`。首次启动时可选择可选插件，或跳过后在 `/plugins` → Discover 中安装。

## 登录 provider

mare 需要一个模型 provider。内置的 provider 如下：

| Provider 名称 | 凭据 | 环境变量 |
| --- | --- | --- |
| `openai` | ChatGPT 登录（浏览器或设备码），或 API 密钥 | `OPENAI_API_KEY` |
| `anthropic` | API 密钥 | `ANTHROPIC_API_KEY` |
| `google` | API 密钥 | `GEMINI_API_KEY` 或 `GOOGLE_API_KEY` |
| `opencode-go` | API 密钥 | `OPENCODE_API_KEY` |
| `deepseek` | API 密钥 | `DEEPSEEK_API_KEY` |
| `kimi` | API 密钥 | `MOONSHOT_API_KEY` |
| `kimi-code` | API 密钥 | `KIMI_API_KEY` |
| `openrouter` | API 密钥 | `OPENROUTER_API_KEY` |
| `ollama` | 本地服务（`http://localhost:11434/v1`），默认无需密钥 | — |
| `lmstudio` | 本地服务（`http://localhost:1234/v1`），默认无需密钥 | — |

环境变量中的 API 密钥无需登录即可使用。`mare login` 会把凭据保存到 `~/.config/mare/auth.json`（权限 `0600`）；已保存的登录优先于环境变量。

使用 ChatGPT 账号登录 OpenAI：

```sh
mare login openai
```

> 不带参数直接运行 `mare login` 时，会优先尝试当前配置模型对应的 provider；若没有可用默认项，在 TTY 下打开 provider 选择器，无 TTY 时以退出码 `2` 结束。

mare 会打开浏览器。在没有浏览器的机器上（例如通过 SSH），改用设备码：

```sh
mare login openai --device-auth
```

对于使用 API 密钥的 provider，请通过标准输入传入密钥。mare 绝不会在终端上提示输入密钥：

```sh
printenv ANTHROPIC_API_KEY | mare login anthropic --with-api-key
```

查看哪些 provider 已登录，以及每个凭据的来源：

```sh
mare login status
```

用 `mare logout [provider]` 删除凭据。

这一步也可以跳过。没有任何 provider 有凭据时，交互式 UI 会启动首次运行引导：选择 provider、登录并选择默认模型。在会话内用 `/login [provider]` 可以做同样的事。

## 在项目中启动

```sh
cd ~/src/my-project
mare
```

把初始提示词作为参数传入，就能直接以一个问题开始：

```sh
mare "what does this project do?"
```

### 信任项目

项目里有些文件会改变 mare 运行的内容：`.mare/init.lua`、`.mare/plugins`、`.mare/skills`、`.mare/prompts`、`.mare/SYSTEM.md`、`.mare/APPEND_SYSTEM.md` 和 `.agents/skills`。mare 只会在受信任的项目中加载它们。当你在含有其中任何一个、或位于 git 仓库内的目录中启动，且尚未保存过决定时，会出现一个提示：

| 选项 | 效果 |
| --- | --- |
| Trust and load | 记住此文件夹为受信任，并立即加载项目的文件 |
| Trust this session only | 仅在本次会话加载项目文件，不写入 `trust.json` |
| Don't trust | 记住这个决定，永不加载这些文件 |
| Not now（或 Esc） | 本次不加载任何内容，下次再询问 |

提示框会按类别列出发现的资源（含 `.mare/init.lua` 的完整 API 风险提示、插件、技能、提示词模板、系统提示词文件，以及脱敏后的 MCP 服务器摘要）；任意分组超过 6 项时可按 `e` 展开或折叠完整列表。永久信任一个目录也意味着信任之后对这些文件的修改，所以只信任你愿意运行的代码。决定保存在 `~/.local/state/mare/trust.json` 中，并适用于子目录。你也可以在 shell 中决定：

```sh
mare trust            # 信任当前项目（如需指定目录，用 mare trust <dir>）
mare trust --revoke   # revoke it
mare --trust        # trust for this run only, save nothing
mare --no-trust     # distrust for this run only
```

## 提一个问题

输入问题并按 Enter。第一个问题不妨让它解读代码：

```text
How is the configuration loaded? Point me at the files.
```

智能体会用工具读取文件并回答。工具调用会显示在对话记录中。按 Ctrl+O 展开或折叠工具输出和思考内容，按 Ctrl+T 查看完整对话记录。

按 Esc 中断正在运行的一轮。智能体工作时按 Enter 会把消息排入队列，等这一轮结束后再发送；见[工作流](workflows.md#智能体工作时的引导与排队)。

## 批准工具调用

默认的权限预设是 `auto`：智能体可以在工作区和临时目录内写入而无需询问，当某个调用会离开沙箱时，mare 会询问你。想马上看到审批，可以用 `read-only` 预设启动，此时写文件需要你批准：

```sh
mare --permission read-only "add a one-line comment to the top of README.md"
```

> 在操作系统沙箱支持时联网会被阻断；默认 Windows 层级无法阻断网络，因此在执行 shell 或其他命令工具前会询问（除非该次无限制访问已获显式批准）。

审批框会写明工具名称，并提供这些回答：

| 按键 | 回答 |
| --- | --- |
| `y` | 允许一次 |
| `a` | 本会话内允许 |
| `A` | 允许并记住建议的规则（mare 能给出建议时显示） |
| `n` | 拒绝，然后告诉模型原因 |
| Esc | 拒绝这次调用；这一轮继续 |
| Ctrl+C | 拒绝并停止这一轮 |

在会话内用 `/permission`（或 `/permission auto`）切换预设。三种预设是 `read-only`、`auto` 和 `full-access`；`--yolo` 是 `--permission full-access` 的简写。详见[权限](../use/permissions.md)。

## 提及文件并运行命令

输入 `@` 加路径，可以把文件附加到消息中。Tab 补全路径，`@"a path with spaces"` 用于带空格的路径：

```text
Explain the retry logic in @src/client.rs
```

文本文件每个最多附加 100 KiB（每条消息最多 256 KiB）。无法附加的文件会得到一行说明原因的提示，智能体仍然可以用读取工具读它。

以 `!` 开头的行会由你自己运行 shell 命令。命令及其输出会进入上下文，模型在下一轮就能看到。用 `!!` 运行命令则不会向上下文添加任何内容：

```text
!git status --short
!!ls -la
```

## 选择模型

`/model` 会打开选择器。没有凭据的 provider 会被归入需要登录的一组。直接给出模型引用即可切换：

```text
/model openai/gpt-6.1-terra
```

`/effort` 为当前模型设置推理强度。你也可以在启动时用 `-m/--model` 和 `-e/--effort`（或 `MARE_EFFORT`）指定。作为全局覆盖，`effort` 仅限单次调用生效，写在 `init.lua` 中的全局 `effort` 会被忽略；而 `init.lua` 中的 `model` 会覆盖 `~/.local/state/mare/state.json` 里记住的选择。

`/status` 显示版本、模型、会话、token 用量和每个 provider 的登录状态。`/help` 列出所有命令。

## 恢复会话

每次对话都保存在 `~/.local/state/mare/sessions` 下。在输入为空时按 Ctrl+D，或连按两次 Ctrl+C 退出。之后可以回来：

```sh
mare -c               # continue the most recent session in this directory
mare -r               # pick a session from a list
mare -r -2            # the second most recent session in this directory
mare -r my-name       # by name, id or unambiguous id prefix
mare -c --fork-session   # continue in a copy and leave the original alone
```

给 `-c` 或 `-r` 加上 `--all`，可以查看所有目录的会话。在 UI 内，`/resume`、`/new`、`/name <title>` 和 `/fork` 分别完成对应的操作。更多内容见[工作流](workflows.md#继续与分叉会话)。

## 写出你的第一个 init.lua

运行 `mare init` 创建带注释示例的 `~/.config/mare/init.lua`（已有文件会保留），然后编辑它：

```lua
-- ~/.config/mare/init.lua
mare.setup {
  model = "openai/gpt-6.1-terra",
}

-- give long shell commands more time
mare.plugin("tools", { bash_timeout_ms = 300000 })

-- /m is a shortcut for /model
mare.command.alias("m", "model")

-- a grep tool backed by ripgrep (grep and find are not built in)
mare.tool.command {
  name = "grep",
  description = "Search file contents with ripgrep.",
  command = { "rg", "--line-number", "--no-heading", "--max-columns=500", "--regexp={pattern}", "--", "{path}" },
  params = {
    pattern = { type = "string", required = true },
    path = { type = "string", default = "." },
  },
}
```

重启 mare，或在会话内运行 `/reload config`。`mare.setup` 会对未知的选项名和错误的类型给出警告，并附带“你是不是想写”的提示；`/config` 会显示每个选项、它的值以及来自哪里。项目中受信任的 `.mare/init.lua` 会在此文件之后运行。

接下来：

- [工作流](workflows.md)：各种示例。
- [模型](../use/models.md)和[权限](../use/permissions.md)：你刚刚做出的选择。
- [选项](../configure/options.md)和[键位映射](../configure/keymaps.md)：`init.lua` 还能设置什么。
