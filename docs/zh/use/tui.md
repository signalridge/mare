# 交互界面

mare 终端 UI 的工作方式：输入编辑器、键位、斜杠命令、文件提及、shell 命令、排队消息、对话记录和通知。

mare 运行在终端的主屏幕上，而不是备用屏幕。已完成的输出（消息、工具调用、差异）会写入终端正常的回滚缓冲区，所以你可以用终端自带的工具滚动、搜索和复制。只有屏幕底部是动态的：流式回复、小组件、输入框和状态栏。启动方式：

```sh
mare                       # new session
mare "explain this repo"   # new session, first prompt already sent
mare -c                    # continue the last session in this directory
```

## 输入编辑器

输入提示词，按 Enter 发送。

| 按键 | 动作 |
| --- | --- |
| `Enter` | 发送。Enter 前如果以 `\` 结尾，则改为插入换行。 |
| `Shift+Enter`、`Ctrl+J` | 插入换行。 |
| `Left` / `Right`、`Ctrl+B` / `Ctrl+F` | 按字符移动。 |
| `Alt+Left` / `Alt+Right`、`Alt+B` / `Alt+F` | 按单词移动。 |
| `Home` / `End`、`Ctrl+A` / `Ctrl+E` | 行首 / 行尾。 |
| `Up` / `Down`、`Ctrl+P` / `Ctrl+N` | 在行间移动；在第一行或最后一行时，浏览提示词历史。 |
| `Backspace`、`Ctrl+H`；`Delete`、`Ctrl+D` | 删除一个字符。输入为空时按 `Ctrl+D` 退出。 |
| `Ctrl+W`、`Alt+Backspace` | 删除前一个单词。 |
| `Alt+D` | 删除后一个单词。 |
| `Ctrl+U` / `Ctrl+K` | 剪切到行首 / 行尾。 |
| `Ctrl+Y`、`Alt+Y` | 粘贴最近剪切的文本；循环切换更早的剪切内容。 |
| `Ctrl+_` | 撤销输入中的修改。 |
| `Ctrl+G` | 在外部编辑器中编辑输入。 |
| `Up` 之后按 `Ctrl+R` | 搜索提示词历史。 |
| `Tab` | 补全 `@file` 或 `/command`。 |

大段粘贴会在输入框中折叠成占位符，发送时再展开。提示词历史保存在 `~/.local/state/mare/history.jsonl`（或 `$XDG_STATE_HOME/mare` 下）。

并非所有终端都能区分地发送 `Shift+Enter` 和 `Alt+Enter`。运行 `/terminal-setup` 检查你的终端并查看如何修复。

### 外部编辑器

`Ctrl+G` 把输入交给编辑器，编辑器成功退出后再把结果放回来。mare 依次使用 `editor` 选项、`$VISUAL`、`$EDITOR`，最后是 `vi`。值里可以带参数，例如 `code --wait`。

```lua
-- ~/.config/mare/init.lua
mare.setup { editor = "nvim" }
```

### 键位

下面是默认键位。在空输入上按 `?`，或运行 `/hotkeys`，可以查看当前生效的列表，包含你的修改。

| 按键 | 动作 |
| --- | --- |
| `Esc` | 中断智能体。空闲时快速连按两次会打开回退选择器（`/undo`）。 |
| `Ctrl+C` | 关闭菜单、清空草稿、中断正在运行的工作。在空闲的空输入上连按两次则退出。 |
| `Ctrl+D` | 输入为空时退出。 |
| `Ctrl+O` | 在紧凑和展开的工具输出与思考内容之间切换。 |
| `Ctrl+T` | 打开对话记录视图。 |
| `Ctrl+S` | 把输入送入正在运行的这一轮（引导）。 |
| `Alt+Enter` | 把输入作为后续消息排队。在 Windows 上该键是 `Ctrl+Q`。 |
| `Ctrl+V` | 粘贴剪贴板中的图片。在 Windows 上该键是 `Alt+V`。 |
| `Ctrl+B` | 把正在运行的 shell 命令移到后台；没有运行中的命令时，光标左移。 |
| `Ctrl+L` | 重绘屏幕。 |
| `Ctrl+Z` | 挂起 mare（用 `fg` 恢复）。Windows 上不可用。 |
| `Alt+S` | 选择模式：用鼠标在动态区域拖动选择文本；松开即复制。 |
| `?` | 列出所有键位（在空输入上）。 |

可以用 `mare.keymap.set` 重新绑定其中任何一个；见[键位映射](../configure/keymaps.md)。

## 斜杠命令

在输入开头键入 `/` 会打开命令列表，用方向键选择，再按 `Tab` 或 `Enter`。`/help` 列出所有命令，`/commands` 打开一个可浏览的列表并运行你选中的命令，`/skills` 对[技能](context.md)做同样的事。

| 分组 | 命令 |
| --- | --- |
| 会话 | `/new`（`/clear`）、`/resume`（`/sessions`）、`/name`、`/undo`、`/rewind`、`/fork`、`/clone`、`/tree`、`/compact`、`/export`、`/copy`、`/delete`、`/archive` |
| 模型 | `/model`、`/effort`、`/login`、`/logout` |
| 上下文 | `/init`、`/memory`、`/context`、`/add-dir` |
| 安全 | `/permission`（`/permissions`）、`/yolo`、`/approval` |
| 检查 | `/status`、`/usage`、`/diff`、`/jobs`、`/mcp`、`/doctor`、`/config`、`/debug` |
| 界面 | `/theme`、`/statusline`、`/hotkeys`、`/terminal-setup`、`/show-reasoning` |
| 其他 | `/btw`、`/reload`、`/plugins`、`/exit`（`/quit`） |

命令列表取决于启用了哪些插件，所以请以 `/help` 为准查看你安装中的权威列表。完整参考见[斜杠命令](../reference/slash-commands.md)。

你可以在 `init.lua` 中用 `mare.command.alias("m", "model")` 添加自己的短名称。

## 文件提及

输入 `@` 并开始写路径。`Tab` 会在工作目录以及用 `/add-dir` 添加的任何目录下的文件中做模糊匹配并补全。

```text
review @src/main.rs and compare it with @"docs/old design.md"
```

发送时，mare 会把每个被提及文件的内容附加到你的消息上。带空格的路径使用 `@"..."`。目录会作为列表附加。无法附加的引用不会被悄悄丢弃：消息中会多出一行说明原因（缺失、二进制、过大、被拒绝）并指向 `read` 工具的提示。限制是每个文件 100 KiB、每条消息 256 KiB；两者都是 `file-ref` 插件的选项（`max_bytes`、`max_total_bytes`）。

```lua
mare.plugin("file-ref", { max_bytes = 200 * 1024 })
```

## Shell 命令

在空输入上键入 `!` 进入 shell 模式，然后输入命令：

```text
!git status           runs the command; command and output go into the context
!!cat .env.example    runs the command; the output is shown but not added to the context
```

命令使用与智能体 `bash` 工具相同的已配置 shell 程序和 prelude，但它是一个独立的进程：不共享智能体 shell 的状态（工作目录、导出的变量），也不适用 `bash` 工具的 `env_include` 和 `env_exclude` 策略。`Esc` 或 `Ctrl+C` 可中止它。在空的 shell 提示符上按 Backspace 会退出 shell 模式。如果智能体正忙，输出会在当前工具结果之后送达，类似一条引导消息。模型最多看到输出的 2000 行或 50 KiB（中间部分会被截去）。

你自己输入的 shell 命令不会经过审批提示，因为它们是你的命令。策略涵盖的范围见[权限](permissions.md)。

## 智能体运行时的操作

智能体工作时，你可以继续输入。

- `Enter` 把你的消息排入队列。它会显示在输入框上方，直到智能体取走，然后变成一条普通用户消息。
- `Ctrl+S` 引导：文本被注入正在运行的这一轮，并在当前工具调用结束后送达。
- `Alt+Enter` 把消息作为后续消息排队，效果与忙碌时按 `Enter` 相同。
- 输入为空时，`Up` 选中一条排队的消息；`Up` 和 `Down` 在它们之间移动，`Enter` 把选中的消息载入输入框进行编辑，`Delete` 删除它，`Esc` 退出选择。
- 有消息排队时在空输入上按 `Enter` 会打开一个菜单：立即引导、中断并发送，或继续排队。
- `Esc` 中断这一轮。在空闲的空输入上连按两次 `Esc` 会打开 `/undo` 选择器。在 `!cmd` 运行时按 `Esc` 只会中止该命令。

`/btw <question>` 提出一个顺带的问题，答案显示在输入框上方的小组件中，不会加入对话。

## 对话记录视图

`Ctrl+T` 在浮层中打开完整的对话记录，工具输出和思考内容都已展开。按键遵循 `less` 和 Vim：

| 按键 | 动作 |
| --- | --- |
| `j` / `Down`、`k` / `Up` | 滚动一行。 |
| `Space`、`f`、`PageDown`；`b`、`PageUp` | 滚动一页。 |
| `g` / `Home`、`G` / `End` | 顶部 / 底部。 |
| `/`、`n`、`N` | 搜索、下一个匹配、上一个匹配。 |
| `y` | 复制阅读位置处的消息。 |
| `q`、`Ctrl+T` | 关闭。 |
| `Esc`、`Ctrl+C` | 清除搜索，然后关闭。 |

已经写入回滚缓冲区的输出无法重绘，所以对旧的工具调用按 `Ctrl+O` 会在对话记录视图中打开它，并滚动到该调用。在调用仍在进行时按 `Ctrl+O`，则会切换之后所有输出的紧凑与展开状态。

## 图片

`Ctrl+V`（Windows 上为 `Alt+V`）把剪贴板中的图片附加到下一条消息。在支持图像协议（Kitty、iTerm2、sixel）的终端上，粘贴的图片和工具返回的图片会内嵌绘制；其他终端的对话记录显示 `[image]`。可以通过 `images` 插件的 `user`、`read`、`tools`、`max_rows` 和 `max_cols` 选项关闭内嵌绘制或改变大小。

## 状态栏

输入框下方的栏显示权限模式、模型和强度、上下文仪表、工作目录、git 分支及其拉取请求（通过 `gh`）、已更改的文件、费用和配额。`/statusline` 切换各个片段，并把选择保存到 `state.json`。如果想改为运行你自己的命令，把一个 JSON 对象通过标准输入交给它，并显示它输出的第一行：

```lua
mare.plugin("statusline", { command = "~/bin/sl" })
```

布局和主题见 [UI](../configure/ui.md)。

## 通知

当耗时至少五秒的一轮结束，或 mare 在等你（一个问题或一次审批）时，若终端未获得焦点，它会发送终端通知（OSC 9、777 或 99，或响铃）。用 `notify` 插件调节：

```lua
mare.plugin("notify", { min_turn_ms = 10000, only_unfocused = true })
-- or run a program with a JSON event on stdin:
mare.plugin("notify", { cmd = { "notify-send-mare" } })
```

`cmd` 在无界面和 RPC 模式下也会运行。设置 `enabled = false` 可关闭该插件。
